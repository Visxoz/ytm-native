//! The user's own music: the audio files in the folders they add, with the
//! title, artists, album and cover art their tags give.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, PoisonError, RwLock},
    time::{Duration, Instant, SystemTime},
};

use serde::{Deserialize, Serialize};
use symphonia::core::{
    errors::Error,
    formats::{FormatOptions, FormatReader, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::{MetadataRevision, StandardTag, StandardVisualKey, Tag, Visual},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
        FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
        GetDriveTypeW,
    },
    System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject},
};

use crate::api::{ArtistRef, Track};

/// What the decoders built in play.
const EXTENSIONS: [&str; 9] = [
    "flac", "m4a", "mka", "mp3", "oga", "ogg", "opus", "wav", "webm",
];

/// `FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM`, both set on folders such
/// as `$RECYCLE.BIN` that hold no music of the user's. System alone is often
/// set on an album's folder to give it a picture.
const HIDDEN_AND_SYSTEM: u32 = 0x2 | 0x4;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, on a file OneDrive keeps online
/// only, which reading would download.
const ONLINE_ONLY: u32 = 0x0040_0000;

/// `DRIVE_REMOVABLE`, a USB stick or a card, which a folder being watched
/// would keep from being safely removed.
const REMOVABLE: u32 = 2;
/// A burst of changes, as an album copied in, is waited out this long...
const SETTLE: Duration = Duration::from_millis(500);
/// ...but no longer than this, so a folder written to all the time still shows.
const SETTLE_AT_MOST: Duration = Duration::from_secs(10);
/// How often a folder that couldn't be watched is tried again.
const RETRY_EVERY: Duration = Duration::from_secs(60);
/// `MAXIMUM_WAIT_OBJECTS`, less the stop event's place.
const MAX_WATCHED: usize = 63;

static FOLDERS: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// The folders `owns` allows, kept in step with the settings.
pub fn set_folders(folders: &[PathBuf]) {
    *FOLDERS.write().unwrap_or_else(PoisonError::into_inner) = folders.to_vec();
}

pub fn folders() -> Vec<PathBuf> {
    FOLDERS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Whether `path` is in a folder the user added. Paths anywhere else, as
/// YouTube's data might hold, are never read, so no share the user didn't
/// pick is ever reached.
pub fn owns(path: &Path) -> bool {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    (FOLDERS.read().unwrap_or_else(PoisonError::into_inner))
        .iter()
        .any(|folder| path.starts_with(folder))
}

/// Which of the local tracks `paths` have lost their files: missing while
/// the folder they're in is there, so an unplugged drive or a sleeping share
/// loses nothing. Blocking.
pub fn gone(paths: HashSet<String>) -> HashSet<String> {
    if paths.is_empty() {
        return paths;
    }
    gone_from(paths, &folders())
}

fn gone_from(mut paths: HashSet<String>, folders: &[PathBuf]) -> HashSet<String> {
    let reachable: Vec<&PathBuf> = folders
        .iter()
        .filter(|f| matches!(f.try_exists(), Ok(true)))
        .collect();
    paths.retain(|path| {
        let path = Path::new(path);
        reachable.iter().any(|f| path.starts_with(f)) && matches!(path.try_exists(), Ok(false))
    });
    paths
}

/// The folders to list, without those inside another, which are listed with it.
fn roots(folders: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = folders.to_vec();
    roots.sort();
    roots.dedup_by(|inner, outer| inner.starts_with(outer));
    roots
}

/// An event the watcher thread waits on, closed once neither side holds it.
struct Event(HANDLE);

// SAFETY: an event handle may be set and waited on from any thread.
unsafe impl Send for Event {}
// SAFETY: as above; nothing changes the handle itself.
unsafe impl Sync for Event {}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed only here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Watches the folders for files added, removed, renamed or written to, until dropped.
pub struct Watcher(Arc<Event>);

impl Drop for Watcher {
    /// Not joined: the thread may be waiting on a sleeping share, and ends
    /// by itself once that answers.
    fn drop(&mut self) {
        // SAFETY: the event stays open while this holds it.
        unsafe { SetEvent(self.0.0) };
    }
}

/// Runs `changed` on a thread of its own once files in `folders` change, a
/// burst of changes at a time. The folders are opened there, as a share
/// that's asleep can take a while to answer.
pub fn watch(folders: &[PathBuf], changed: impl Fn() + Send + 'static) -> Option<Watcher> {
    let roots = roots(folders);
    if roots.is_empty() {
        return None;
    }
    // SAFETY: a manual-reset event, not yet set, with no name or attributes.
    let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if stop.is_null() {
        return None;
    }
    let stop = Arc::new(Event(stop));
    let waiting = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("folder-watch".into())
        .spawn(move || wait_for_changes(waiting.0, &roots, changed));
    if let Err(e) = spawned {
        log::warn!("could not watch the music folders: {e}");
        return None;
    }
    Some(Watcher(stop))
}

fn open_watch(root: &Path) -> Option<HANDLE> {
    let path: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `path` is NUL-terminated.
    let handle = unsafe {
        FindFirstChangeNotificationW(
            path.as_ptr(),
            1,
            FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_LAST_WRITE,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        log::debug!(
            "could not watch a music folder: {}",
            io::Error::last_os_error()
        );
        return None;
    }
    Some(handle)
}

/// The watcher's caller keeps `stop` open while it runs.
fn stopped(stop: HANDLE) -> bool {
    // SAFETY: as above.
    unsafe { WaitForSingleObject(stop, 0) == WAIT_OBJECT_0 }
}

fn close_watches(watches: &[HANDLE]) {
    for &handle in watches {
        // SAFETY: each came from FindFirstChangeNotificationW and is closed once.
        unsafe { FindCloseChangeNotification(handle) };
    }
}

fn wait_for_changes(stop: HANDLE, roots: &[PathBuf], changed: impl Fn()) {
    // shortcut: folders on removable drives aren't watched, so a USB stick can be
    // safely removed; a USB hard disk counts as fixed. And as Windows won't rename
    // a folder with an open handle inside it, a watched folder's parents can't be
    // renamed or moved while Lily runs. Use device notifications or polling should
    // either matter.
    let mut roots: Vec<&Path> = (roots.iter().map(PathBuf::as_path))
        .filter(|root| !removable(root))
        .collect();
    if roots.len() > MAX_WATCHED {
        log::warn!("only the first {MAX_WATCHED} music folders are watched");
        roots.truncate(MAX_WATCHED);
    }
    // The stop event, then a watch for each of `watched`, in order.
    let mut handles = vec![stop];
    let mut watched = Vec::new();
    let mut unwatched = Vec::new();
    for root in roots {
        if stopped(stop) {
            break;
        }
        match open_watch(root) {
            Some(handle) => {
                handles.push(handle);
                watched.push(root);
            }
            None => unwatched.push(root),
        }
    }
    // When the first change not yet reported came.
    let mut since: Option<Instant> = None;
    loop {
        let timeout = match since {
            Some(at) => {
                let left = SETTLE_AT_MOST.saturating_sub(at.elapsed());
                if left.is_zero() {
                    since = None;
                    changed();
                    continue;
                }
                left.min(SETTLE).as_millis() as u32
            }
            None if watched.is_empty() && unwatched.is_empty() => break,
            None => RETRY_EVERY.as_millis() as u32,
        };
        // SAFETY: every handle is open, and there are at most `MAX_WATCHED` + 1.
        let result =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout) };
        if result == WAIT_TIMEOUT {
            if since.take().is_some() {
                changed();
                continue;
            }
            // A folder renamed or sent to the Recycle Bin is watched where it
            // went, so it is let go of, to be watched again should it come back.
            let mut moved = false;
            for i in (0..watched.len()).rev() {
                if matches!(watched[i].try_exists(), Ok(false)) {
                    close_watches(&[handles.swap_remove(i + 1)]);
                    unwatched.push(watched.swap_remove(i));
                    moved = true;
                }
            }
            // A share back from sleep, or a drive plugged in again; whatever
            // changed while it was away is reported.
            let before = unwatched.len();
            unwatched.retain(|root| {
                if stopped(stop) {
                    return true;
                }
                match open_watch(root) {
                    Some(handle) => {
                        handles.push(handle);
                        watched.push(root);
                        false
                    }
                    None => true,
                }
            });
            if moved || unwatched.len() < before {
                changed();
            }
            continue;
        }
        let index = result.wrapping_sub(WAIT_OBJECT_0) as usize;
        if index == 0 {
            break;
        }
        if index >= handles.len() {
            log::warn!(
                "stopped watching the music folders: {}",
                io::Error::last_os_error()
            );
            break;
        }
        since.get_or_insert_with(Instant::now);
        // SAFETY: the handle came from FindFirstChangeNotificationW.
        if unsafe { FindNextChangeNotification(handles[index]) } == 0 {
            // Its folder went, or its drive, or a share's connection: it's
            // reported, and watched again once it can be.
            close_watches(&[handles.swap_remove(index)]);
            unwatched.push(watched.swap_remove(index - 1));
        }
    }
    close_watches(&handles[1..]);
}

fn removable(path: &Path) -> bool {
    let mut components = path.components();
    let (Some(Component::Prefix(prefix)), Some(Component::RootDir)) =
        (components.next(), components.next())
    else {
        return false;
    };
    let root: Vec<u16> = prefix
        .as_os_str()
        .encode_wide()
        .chain([u16::from(b'\\'), 0])
        .collect();
    // SAFETY: `root` is NUL-terminated.
    unsafe { GetDriveTypeW(root.as_ptr()) == REMOVABLE }
}

fn library_path() -> PathBuf {
    crate::settings::data_dir().join("local.json")
}

/// A file as read, with what its track sorts and shares a cover by.
#[derive(Serialize, Deserialize)]
struct Song {
    track: Track,
    /// The album artist, or else the track's own.
    by: String,
    disc: u16,
    cover: bool,
}

/// Each file's song as last read, by path, with when it was modified then,
/// as time since 1970, so a scan reads only files that are new or changed.
/// A file that won't open is kept as `None`, so it isn't tried again until
/// it changes.
#[derive(Default, Serialize, Deserialize)]
pub struct Library {
    files: HashMap<PathBuf, (Duration, Option<Song>)>,
    /// The online-only files the last scan listed, which `files` leaves out.
    #[serde(skip)]
    online: HashSet<String>,
    /// The folders the last scan couldn't list, whose files it left out.
    #[serde(skip)]
    away: HashSet<PathBuf>,
    /// Counts the scans that found a change, so a list shown can tell it's old.
    #[serde(skip)]
    revision: u64,
}

impl Library {
    pub fn load() -> Self {
        crate::settings::read_json(&library_path()).unwrap_or_default()
    }

    pub fn save(&self) {
        crate::settings::save_json(&library_path(), self, "the local files");
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The tracks in `folders` and below, by album artist, album, disc and
    /// track number, and whether any file was new, changed or gone. Blocking.
    pub fn scan(&mut self, folders: &[PathBuf]) -> (Vec<Track>, bool) {
        let mut found = HashMap::with_capacity(self.files.len());
        let mut kept = Vec::new();
        let mut away = HashSet::new();
        // Listed by name and not kept, so they're read once downloaded.
        let mut online = Vec::new();
        let mut changed = false;
        let roots = roots(folders);
        let mut dirs = roots.clone();
        while let Some(dir) = dirs.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) => {
                    log::debug!("could not list a music folder: {e}");
                    // An unplugged drive or a sleeping share keeps what was
                    // read of it, so it isn't all read again once it's back.
                    if roots.contains(&dir) {
                        let before = kept.len();
                        kept.extend(self.files.extract_if(|path, _| path.starts_with(&dir)));
                        if kept.len() > before {
                            away.insert(dir.clone());
                        }
                    }
                    continue;
                }
            };
            for entry in entries.flatten() {
                // Links aren't followed, so a loop of them can't trap the scan.
                let (Ok(kind), Ok(metadata)) = (entry.file_type(), entry.metadata()) else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    if metadata.file_attributes() & HIDDEN_AND_SYSTEM != HIDDEN_AND_SYSTEM {
                        dirs.push(path);
                    }
                    continue;
                }
                // Paths that aren't Unicode can't be a track's ID.
                if !kind.is_file() || !is_audio(&path) || path.to_str().is_none() {
                    continue;
                }
                let Ok(modified) = metadata.modified() else {
                    continue;
                };
                // A file dated before 1970 reads as 1970.
                let modified =
                    (modified.duration_since(SystemTime::UNIX_EPOCH)).unwrap_or_default();
                let song = match self.files.remove(&path) {
                    Some((at, song)) if at == modified => song,
                    _ if metadata.file_attributes() & ONLINE_ONLY != 0 => {
                        online.extend(song(&path, Tags::default(), false, None));
                        continue;
                    }
                    _ => match read_song(&path) {
                        Ok(song) => {
                            changed = true;
                            song
                        }
                        // Not kept, so it's tried again next time.
                        Err(e) => {
                            log::debug!("could not read an audio file: {e}");
                            continue;
                        }
                    },
                };
                found.insert(path, (modified, song));
            }
        }
        changed |= !self.files.is_empty();
        let online_now: HashSet<String> = online.iter().map(|s| s.track.id.clone()).collect();
        // Their songs aren't listed while they're away, nor kept in `files`.
        changed |= online_now != self.online || away != self.away;
        self.online = online_now;
        self.away = away;
        self.revision += u64::from(changed);
        self.files = found;
        let songs = self.files.values().filter_map(|(_, s)| s.as_ref());
        let tracks = tracks(songs.chain(&online));
        self.files.extend(kept);
        (tracks, changed)
    }
}

/// `songs` in order as tracks.
fn tracks<'a>(songs: impl Iterator<Item = &'a Song>) -> Vec<Track> {
    let mut songs: Vec<&Song> = songs.collect();
    songs.sort_by_cached_key(|&s| {
        (
            s.by.to_lowercase(),
            s.track.album_name().to_lowercase(),
            s.disc,
            s.track.track_nr,
            s.track.title.to_lowercase(),
            // So files alike keep their places from one scan to the next.
            s.track.id.as_str(),
        )
    });
    let mut tracks = Vec::with_capacity(songs.len());
    let same_album = |a: &&Song, b: &&Song| {
        a.track.album.is_some() && a.by == b.by && a.track.album == b.track.album
    };
    for album in songs.chunk_by(same_album) {
        // One file's cover for the whole album, so it is read and decoded once.
        let cover = album.iter().find(|s| s.cover).map(|s| s.track.id.clone());
        tracks.extend(album.iter().map(|s| Track {
            thumb: cover.clone(),
            ..s.track.clone()
        }));
    }
    tracks
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// The file's reader, or `None` should it not be audio. A file that can't be
/// read just now, as on a share that dropped, is an error.
fn probe(path: &Path) -> io::Result<Option<Box<dyn FormatReader>>> {
    let file = File::open(path)?;
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let probe = symphonia::default::get_probe();
    match probe.probe(&hint, stream, FormatOptions::default(), Default::default()) {
        Ok(format) => Ok(Some(format)),
        // Running out is what a file that isn't audio does.
        Err(Error::IoError(e)) if e.kind() != io::ErrorKind::UnexpectedEof => Err(e),
        Err(e) => {
            log::debug!("unreadable audio file: {e}");
            Ok(None)
        }
    }
}

/// Every revision of the file's metadata, oldest first: tags found ahead of
/// the container, such as an MP3's ID3, then the container's own.
fn revisions(format: &mut dyn FormatReader, mut each: impl FnMut(&MetadataRevision)) {
    let mut metadata = format.metadata();
    loop {
        if let Some(revision) = metadata.current() {
            each(revision);
        }
        if metadata.pop().is_none() {
            break;
        }
    }
}

fn tags(revision: &MetadataRevision) -> impl Iterator<Item = &Tag> {
    let per_track = revision.per_track.iter().flat_map(|t| &t.metadata.tags);
    revision.media.tags.iter().chain(per_track)
}

fn visuals(revision: &MetadataRevision) -> impl Iterator<Item = &Visual> {
    let per_track = revision.per_track.iter().flat_map(|t| &t.metadata.visuals);
    revision.media.visuals.iter().chain(per_track)
}

#[derive(Default)]
struct Tags {
    title: Option<String>,
    artists: Vec<String>,
    album_artist: Option<String>,
    album: Option<String>,
    track_nr: Option<u16>,
    disc: Option<u16>,
}

impl Tags {
    /// Keeps the first of each, as later revisions repeat earlier ones.
    fn add(&mut self, tag: &Tag) {
        match &tag.std {
            Some(StandardTag::TrackTitle(s)) => {
                self.title.get_or_insert_with(|| s.trim().to_owned());
            }
            Some(StandardTag::Artist(s)) => {
                let name = s.trim();
                if !name.is_empty() && !self.artists.iter().any(|a| a == name) {
                    self.artists.push(name.to_owned());
                }
            }
            Some(StandardTag::AlbumArtist(s)) => {
                self.album_artist.get_or_insert_with(|| s.trim().to_owned());
            }
            Some(StandardTag::Album(s)) => {
                self.album.get_or_insert_with(|| s.trim().to_owned());
            }
            Some(StandardTag::TrackNumber(n)) => {
                self.track_nr = self.track_nr.or(u16::try_from(*n).ok());
            }
            Some(StandardTag::DiscNumber(n)) => {
                self.disc = self.disc.or(u16::try_from(*n).ok());
            }
            _ => {}
        }
    }
}

/// The file as a song. Its cover is only noted here, and read as it shows.
fn read_song(path: &Path) -> io::Result<Option<Song>> {
    let Some(mut format) = probe(path)? else {
        return Ok(None);
    };
    // Matroska gives the length for the whole file only.
    let track_time = format
        .default_track(TrackType::Audio)
        .and_then(|t| t.time_base?.calc_duration(t.duration?));
    let media = format.media_info();
    let time = track_time.or_else(|| media.time_base?.calc_duration(media.duration?));
    let mut found = Tags::default();
    let mut cover = false;
    revisions(format.as_mut(), |revision| {
        tags(revision).for_each(|t| found.add(t));
        cover |= visuals(revision).next().is_some();
    });
    Ok(song(path, found, cover, time.map(|t| t.as_secs_f64())))
}

/// The song `found` tells of: its path is its ID, and an untagged file goes
/// by its name.
fn song(path: &Path, found: Tags, cover: bool, seconds: Option<f64>) -> Option<Song> {
    let id = path.to_str()?.to_owned();
    let present = |s: Option<String>| s.filter(|s| !s.is_empty());
    let title =
        present(found.title).or_else(|| Some(path.file_stem()?.to_string_lossy().into_owned()))?;
    let album_artist = present(found.album_artist);
    let mut artists = found.artists;
    if artists.is_empty() {
        artists.extend(album_artist.clone());
    }
    let track = Track {
        title,
        artists: artists
            .into_iter()
            .map(|name| ArtistRef { id: None, name })
            .collect(),
        // Named for itself, so tracks of one album follow on gaplessly.
        album: present(found.album).map(|name| (name.clone(), name)),
        duration: seconds.map(|s| s.round() as u32),
        thumb: None,
        track_nr: found.track_nr,
        id,
        video: false,
        set_video_id: None,
    };
    Some(Song {
        by: album_artist.unwrap_or_else(|| track.artist_names()),
        disc: found.disc.unwrap_or(0),
        cover,
        track,
    })
}

/// A picture, or the front cover inside an audio file. Blocking.
pub fn artwork(path: &Path) -> Option<Vec<u8>> {
    if !is_audio(path) {
        return std::fs::read(path).ok();
    }
    let mut format = probe(path).ok().flatten()?;
    let mut cover: Option<Vec<u8>> = None;
    let mut front = false;
    revisions(format.as_mut(), |revision| {
        for visual in visuals(revision) {
            let is_front = visual.usage == Some(StandardVisualKey::FrontCover);
            if cover.is_none() || (is_front && !front) {
                cover = Some(visual.data.to_vec());
                front = is_front;
            }
        }
    });
    cover
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A silent mono 16-bit WAV at 8 kHz, untagged.
    pub(crate) fn write_wav(path: &Path, secs: u32) {
        const RATE: u32 = 8000;
        let data = vec![0; (RATE * secs * 2) as usize];
        let mut wav = b"RIFF".to_vec();
        wav.extend((36 + data.len() as u32).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes()); // PCM
        wav.extend(1u16.to_le_bytes()); // mono
        wav.extend(RATE.to_le_bytes());
        wav.extend((RATE * 2).to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend((data.len() as u32).to_le_bytes());
        wav.extend(data);
        std::fs::write(path, wav).unwrap();
    }

    #[test]
    fn reads_an_untagged_file_by_its_name() {
        let dir = std::env::temp_dir().join(format!("lily-untagged-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Some Song.wav");
        write_wav(&path, 2);
        let (tracks, _) = Library::default().scan(std::slice::from_ref(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        let [track] = tracks.as_slice() else {
            panic!("one track, found {}", tracks.len());
        };
        assert_eq!(track.id, path.to_str().unwrap());
        assert_eq!(track.title, "Some Song");
        assert_eq!(track.duration, Some(2));
        assert!(track.artists.is_empty() && track.album.is_none());
        assert_eq!(track.thumb, None, "no cover to read");
    }

    fn tagged(id: &str, by: &str, album: Option<&str>, disc: u16, nr: u16, cover: bool) -> Song {
        Song {
            track: Track {
                id: id.into(),
                album: album.map(|a| (a.into(), a.into())),
                track_nr: Some(nr),
                ..Default::default()
            },
            by: by.into(),
            disc,
            cover,
        }
    }

    #[test]
    fn orders_albums_by_disc_and_shares_their_cover() {
        let songs = [
            tagged("2-1", "Band", Some("Album"), 2, 1, true),
            tagged("1-2", "Band", Some("Album"), 1, 2, false),
            tagged("1-1", "band", Some("Album"), 1, 1, false),
            tagged("loose", "Band", None, 0, 1, true),
            tagged("plain", "Band", None, 0, 2, false),
        ];
        let tracks = tracks(songs.iter());
        let order: Vec<_> = tracks
            .iter()
            .map(|t| (t.id.as_str(), t.thumb.as_deref()))
            .collect();
        assert_eq!(
            order,
            [
                ("loose", Some("loose")),
                ("plain", None),
                // Named differently, so not the same album's cover.
                ("1-1", None),
                ("1-2", Some("2-1")),
                ("2-1", Some("2-1")),
            ]
        );
    }

    #[test]
    fn owns_only_the_folders_added() {
        set_folders(&[PathBuf::from(r"C:\Music")]);
        assert!(owns(Path::new(r"C:\Music\Band\a.mp3")));
        assert!(!owns(Path::new(r"C:\Music\..\Windows\win.ini")));
        assert!(!owns(Path::new(r"C:\Musicals\a.mp3")));
        assert!(!owns(Path::new(r"\\host\share\a.jpg")));
        assert!(!owns(Path::new("//yt3.ggpht.com/a")));
    }

    #[test]
    fn a_file_is_gone_only_from_a_folder_still_there() {
        let dir = std::env::temp_dir().join(format!("lily-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("here.wav"), b"").unwrap();
        let unplugged = dir.with_extension("unplugged");
        let path = |p: &Path| p.to_str().unwrap().to_owned();
        let paths = HashSet::from([
            path(&dir.join("here.wav")),
            path(&dir.join("deleted.wav")),
            path(&unplugged.join("away.wav")),
        ]);
        let gone = gone_from(paths, &[dir.clone(), unplugged]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(gone, HashSet::from([path(&dir.join("deleted.wav"))]));
    }

    #[test]
    fn watches_a_folder_for_files_going() {
        let dir = std::env::temp_dir().join(format!("lily-watch-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("album")).unwrap();
        let file = dir.join("album").join("song.wav");
        std::fs::write(&file, b"").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = watch(std::slice::from_ref(&dir), move || {
            let _ = tx.send(());
        })
        .unwrap();
        // The folder is opened on the watcher's thread, so a change made
        // before it is watched goes unseen; one is made until it shows.
        let reported = (0..30).any(|_| {
            std::fs::write(&file, b"").unwrap();
            std::fs::remove_file(&file).unwrap();
            rx.recv_timeout(Duration::from_millis(500)).is_ok()
        });
        drop(watcher);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(reported, "a file deleted below the folder is reported");
    }

    #[test]
    fn knows_audio_by_its_extension() {
        assert!(is_audio(Path::new(r"C:\Music\a.FLAC")));
        assert!(is_audio(Path::new(r"C:\Music\b.mp3")));
        assert!(!is_audio(Path::new(r"C:\Music\cover.jpg")));
        assert!(!is_audio(Path::new(r"C:\Music\notes")));
    }

    #[test]
    fn a_local_track_never_passes_for_a_youtube_one() {
        let track = Track {
            id: r"C:\Music\ab.mp3".into(),
            album: Some(("Album".into(), "Album".into())),
            ..Default::default()
        };
        assert!(track.is_local());
        assert!(!crate::api::is_video_id(&track.id));
        assert_eq!(track.album_id(), None, "no YouTube album to open");
        let youtube = Track {
            id: "dQw4w9WgXcQ".into(),
            ..Default::default()
        };
        assert!(!youtube.is_local());
        assert!(crate::api::is_video_id(&youtube.id));
    }

    #[test]
    fn rescans_only_what_changed() {
        let dir = std::env::temp_dir().join(format!("lily-local-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        // Not audio, so it reads as nothing and is kept as such.
        std::fs::write(dir.join("inner").join("broken.mp3"), b"not audio").unwrap();
        std::fs::write(dir.join("cover.jpg"), b"jpeg").unwrap();
        let mut library = Library::default();
        let folders = [dir.clone(), dir.join("inner")];
        assert_eq!(library.scan(&folders), (Vec::new(), true));
        assert_eq!(library.files.len(), 1, "found once, though in two folders");
        assert_eq!(library.scan(&folders), (Vec::new(), false), "nothing new");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            library.scan(&folders),
            (Vec::new(), true),
            "its songs aren't listed while it's away"
        );
        assert_eq!(
            library.files.len(),
            1,
            "a folder that is away keeps its files"
        );
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        assert_eq!(library.scan(&folders), (Vec::new(), true));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(library.files.is_empty(), "a file gone is forgotten");
    }
}
