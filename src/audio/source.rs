//! Downloads a stream into memory in the background and serves it to the
//! demuxer as a seekable file, blocking reads until their bytes arrive. The
//! download follows the reader, so a seek ahead is fetched next and the gaps
//! after. A file on disk is served in place.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::windows::fs::FileExt,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use symphonia::core::io::MediaSource;

/// googlevideo throttles large single requests, so the file is fetched in
/// ranges no bigger than this, each within one aligned block.
const CHUNK: u64 = 1024 * 1024;
const READ_BUF: usize = 64 * 1024;
/// Streams longer than this, which only long mixes and uploads reach, keep
/// only the blocks around the read position; songs are held whole, for replays.
const WINDOW_FROM: u64 = 64 * CHUNK;
/// Blocks a window keeps behind the read position, and fetches ahead of it.
const KEEP_BEHIND: usize = 8;
const KEEP_AHEAD: usize = 24;
const MAX_RETRIES: u32 = 4;
/// How long a connection that stops answering is retried, so one that
/// comes back within it plays on.
const RIDE_OUT: Duration = Duration::from_secs(60);
const RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);
/// Fresh URLs asked for in a row without a whole range arriving from them:
/// the same client once more, then each of the others.
const MAX_REFRESHES: u32 = crate::streams::CLIENT_COUNT as u32;

pub struct Source {
    pub url: String,
    /// Length YouTube reports up front, or 0 if it did not.
    pub size: u64,
    pub headers: Vec<(String, String)>,
    /// Its first bytes, fetched ahead from the same URL, or none.
    pub head: Vec<u8>,
}

/// Asks YouTube again for the stream, as googlevideo sometimes serves a URL's
/// first megabyte and refuses the rest. Told whether the last fresh URL was
/// refused too. Runs on the download thread, so may block.
pub type Refresh = Box<dyn FnMut(bool) -> Result<Source, String> + Send>;

#[derive(Default)]
struct State {
    /// The stream by `CHUNK`, each block held from its start.
    blocks: Vec<Vec<u8>>,
    total: u64,
    /// Where the reader last read, which the download serves first.
    pos: u64,
    /// Keeps only the blocks around `pos`, as the stream is too long to hold.
    window: bool,
    readers: usize,
    /// Every reader has gone, so a window has nothing left to follow.
    abandoned: bool,
    /// Every byte is held.
    done: bool,
    cancelled: bool,
    error: Option<String>,
    /// The error is the connection's, which outlasted `RIDE_OUT`.
    dropped: bool,
}

impl State {
    fn held(&self, block: usize) -> usize {
        self.blocks.get(block).map_or(0, Vec::len)
    }

    /// The end of the bytes held unbroken from `pos`, or `pos` if its byte is missing.
    fn run_end(&self, pos: u64) -> u64 {
        let mut block = (pos / CHUNK) as usize;
        while self.held(block) == CHUNK as usize {
            block += 1;
        }
        (block as u64 * CHUNK + self.held(block) as u64).max(pos)
    }

    /// Where the next range starts: the read position's block and on from
    /// it, then any gap from the start. `None` once all a window may hold is.
    fn next_range(&self) -> Option<u64> {
        if self.total == 0 {
            return Some(self.run_end(0));
        }
        let count = self.total.div_ceil(CHUNK) as usize;
        let from = ((self.pos / CHUNK) as usize).min(count - 1);
        let (ahead, behind) = if self.window {
            ((from + KEEP_AHEAD).min(count), 0)
        } else {
            (count, from)
        };
        let missing =
            |&b: &usize| (self.held(b) as u64) < (self.total - b as u64 * CHUNK).min(CHUNK);
        (from..ahead)
            .chain(0..behind)
            .find(missing)
            .map(|b| b as u64 * CHUNK + self.held(b) as u64)
    }

    /// Drops a window's blocks away from the read position, to be fetched again if read.
    fn evict(&mut self) {
        for b in 0..self.blocks.len() {
            if !self.kept(b) {
                self.blocks[b] = Vec::new();
            }
        }
    }

    /// Whether block `b` is within the window around the read position.
    fn kept(&self, b: usize) -> bool {
        let from = (self.pos / CHUNK) as usize;
        !self.window || (b + KEEP_BEHIND >= from && b < from + KEEP_AHEAD)
    }

    /// Adds `bytes` at `at`, which follows what its block holds.
    fn put(&mut self, mut at: u64, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let b = (at / CHUNK) as usize;
            if self.blocks.len() <= b {
                self.blocks.resize_with(b + 1, Vec::new);
            }
            let size = match self.total {
                0 => CHUNK,
                total => total.saturating_sub(b as u64 * CHUNK).min(CHUNK),
            } as usize;
            let block = &mut self.blocks[b];
            debug_assert_eq!(at, b as u64 * CHUNK + block.len() as u64);
            // Sized once, as growing by doubling can hold twice the block.
            block.reserve_exact(size.saturating_sub(block.len()));
            let n = bytes.len().min(CHUNK as usize - block.len());
            block.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            at += n as u64;
        }
    }
}

#[derive(Default)]
pub struct Download {
    state: Mutex<State>,
    changed: Condvar,
    /// A file read in place, set before `done`.
    file: OnceLock<File>,
    /// The URL it started from, which a replay is matched on; empty for a file.
    url: String,
}

impl Download {
    fn sized(size: u64) -> Self {
        let download = Self::default();
        download.state.lock().unwrap().total = size;
        download
    }

    pub fn start(mut source: Source, refresh: Refresh) -> Arc<Self> {
        let mut download = Self {
            url: source.url.clone(),
            ..Self::sized(source.size)
        };
        let state = download.state.get_mut().unwrap();
        // Only here, as `fetch_all` must keep every byte.
        state.window = source.size > WINDOW_FROM;
        // `run` carries on after the bytes held. Taken, as the source is kept
        // for refreshes all song long.
        state.put(0, &std::mem::take(&mut source.head));
        let download = Arc::new(download);
        let worker = Arc::clone(&download);
        thread::Builder::new()
            .name("stream-download".into())
            .spawn(move || worker.run(source, refresh))
            .expect("download thread spawns");
        download
    }

    /// Serves `path`; a file that won't open fails the first read. Opened on
    /// a thread, as a sleeping disk or share can take seconds to answer.
    pub fn open(path: PathBuf) -> Arc<Self> {
        let download = Arc::new(Self::default());
        let worker = Arc::clone(&download);
        thread::Builder::new()
            .name("file-open".into())
            .spawn(move || {
                let opened = File::open(&path).and_then(|f| Ok((f.metadata()?.len(), f)));
                let mut state = worker.state.lock().unwrap();
                match opened {
                    Ok((len, file)) => {
                        let _ = worker.file.set(file);
                        state.total = len;
                        state.done = true;
                    }
                    Err(e) => state.error = Some(format!("couldn't open the file: {e}")),
                }
                drop(state);
                worker.changed.notify_all();
            })
            .expect("file thread spawns");
        download
    }

    /// The whole stream, fetched on this thread as the player would, for keeping.
    pub fn fetch_all(source: Source, refresh: Refresh) -> Result<Vec<u8>, String> {
        let download = Self::sized(source.size);
        download.run(source, refresh);
        let state = download.state.into_inner().unwrap();
        if let Some(e) = state.error {
            return Err(e);
        }
        let mut data = Vec::with_capacity(state.blocks.iter().map(Vec::len).sum());
        // Each block freed as it is copied, so the stream is not held twice.
        for block in state.blocks {
            data.extend_from_slice(&block);
        }
        Ok(data)
    }

    /// Stops the download and fails any blocked read.
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.cancelled = true;
        // Nothing reads a cancelled download, though a slot or a thread
        // waiting on the network may hold it a while yet.
        state.blocks = Vec::new();
        drop(state);
        self.changed.notify_all();
    }

    /// Where the bytes held from the read position run to, as a fraction of
    /// the stream.
    pub fn progress(&self) -> f32 {
        let state = self.state.lock().unwrap();
        if state.done {
            1.0
        } else if state.total == 0 {
            0.0
        } else {
            state.run_end(state.pos) as f32 / state.total as f32
        }
    }

    /// Whether all of `url` is held in memory, so it can play again from
    /// there. A cancelled download is emptied, even one that was done.
    pub fn whole(&self, url: &str) -> bool {
        let state = self.state.lock().unwrap();
        !self.url.is_empty() && self.url == url && state.done && !state.cancelled
    }

    /// Whether it gave up on a connection that stopped answering, rather
    /// than on a refusal or a bad file.
    pub fn dropped(&self) -> bool {
        self.state.lock().unwrap().dropped
    }

    pub fn reader(self: &Arc<Self>) -> StreamReader {
        self.state.lock().unwrap().readers += 1;
        StreamReader {
            download: Arc::clone(self),
            pos: 0,
        }
    }

    fn run(&self, mut source: Source, mut refresh: Refresh) {
        if let Err(e) = check_host(&source.url) {
            return self.fail(e);
        }
        // Kept apart, so a stray error once a lost connection is back gets
        // its own tries.
        let (mut retries, mut drops, mut refreshes) = (0, 0, 0);
        // From the first failure in a row, not the last byte, as a PC
        // waking from sleep fails at once while its network comes back.
        let mut failing_since = None;
        let mut buf = vec![0; READ_BUF];
        loop {
            let (offset, total) = {
                let mut state = self.state.lock().unwrap();
                loop {
                    if state.cancelled {
                        return;
                    }
                    if state.total > 0 && state.run_end(0) >= state.total {
                        state.done = true;
                        drop(state);
                        self.changed.notify_all();
                        return;
                    }
                    state.evict();
                    if let Some(offset) = state.next_range() {
                        break (offset, state.total);
                    }
                    if state.abandoned {
                        // Frees the window, and fails any late reader at once.
                        drop(state);
                        return self.cancel();
                    }
                    // The window ahead is held, so on once the reader moves.
                    let block = state.pos / CHUNK;
                    state = self
                        .changed
                        .wait_while(state, |s| {
                            !s.cancelled && !s.abandoned && s.pos / CHUNK == block
                        })
                        .unwrap();
                }
            };
            let error = match self.fetch(&source, offset, total, &mut buf) {
                Ok(()) => {
                    retries = 0;
                    drops = 0;
                    refreshes = 0;
                    failing_since = None;
                    continue;
                }
                Err(e) => e,
            };
            {
                let state = self.state.lock().unwrap();
                // Nobody reads a cancelled download, so it neither waits nor asks.
                if state.cancelled {
                    return;
                }
                // A range that broke off after some bytes still got somewhere,
                // so only a stretch with nothing arriving gives up.
                if state.run_end(offset) > offset {
                    retries = 0;
                    drops = 0;
                    failing_since = None;
                }
            }
            let refused = is_refused(&*error);
            if refused && refreshes < MAX_REFRESHES {
                // Asking the same URL again cannot help, but a fresh one
                // carries on from the same byte.
                refreshes += 1;
                match refreshed(&mut refresh, &source, refreshes > 1) {
                    Ok(new) => {
                        source = new;
                        continue;
                    }
                    Err(e) => log::warn!("no fresh URL for a refused download: {e}"),
                }
            } else if is_connection(&*error) {
                let failing = *failing_since.get_or_insert_with(Instant::now);
                if let Some(delay) = retry_delay(drops, failing.elapsed()) {
                    drops += 1;
                    log::debug!("download lost its connection, retrying ({drops}): {error}");
                    self.wait(delay);
                    continue;
                }
                self.state.lock().unwrap().dropped = true;
            } else if !refused && retries < MAX_RETRIES {
                retries += 1;
                log::debug!("download chunk failed, retrying ({retries}): {error}");
                self.wait(RETRY_DELAY * retries);
                continue;
            }
            log::warn!("download failed: {error}");
            return self.fail(error.to_string());
        }
    }

    /// Sleeps for `delay`, or until cancelled.
    fn wait(&self, delay: Duration) {
        let state = self.state.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(state, delay, |s| !s.cancelled);
    }

    /// Ends the download with `error`, which a blocked read then returns.
    fn fail(&self, error: String) {
        self.state.lock().unwrap().error = Some(error);
        self.changed.notify_all();
    }

    /// Fetches one range starting at `offset`, reading through `buf`: to the
    /// end of its block, or all of a stream of unknown length.
    fn fetch(
        &self,
        source: &Source,
        offset: u64,
        total: u64,
        buf: &mut [u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut request = crate::http::blocking_client().get(&source.url);
        for (name, value) in &source.headers {
            request = request.header(name, value);
        }
        let mut end = u64::MAX;
        if total > 0 {
            // Ends on a megabyte, so starting after a prefetched head doesn't
            // push the first range past where cut-off URLs start refusing.
            end = ((offset / CHUNK + 1) * CHUNK).min(total);
            let last = end - 1;
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-{last}"));
        }
        let mut response = request
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(reqwest::Error::without_url)?;
        // Anything but a partial answer starts at the file's first byte,
        // which must not follow the bytes already held.
        if offset > 0 && response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err("the answer does not follow the bytes held".into());
        }
        if total == 0 {
            self.state.lock().unwrap().total = response.content_length().unwrap_or(0);
        }

        let mut at = offset;
        loop {
            if at == end {
                break;
            }
            // Never past the range, as the next block may be held already.
            let room = usize::try_from(end - at).unwrap_or(usize::MAX);
            let n = response.read(buf)?.min(room);
            if n == 0 {
                break;
            }
            let mut state = self.state.lock().unwrap();
            // A seek out of the window would only see this block dropped.
            if state.cancelled || !state.kept((at / CHUNK) as usize) {
                return Ok(());
            }
            state.put(at, &buf[..n]);
            at += n as u64;
            drop(state);
            self.changed.notify_all();
        }
        // An empty answer would be asked again forever.
        if at == offset {
            return Err("the server sent no bytes".into());
        }
        let mut state = self.state.lock().unwrap();
        // All of a stream of unknown length has arrived, so now it is known.
        if state.total == 0 {
            state.total = at;
        }
        Ok(())
    }
}

/// The wait before retrying after `retries` failures in a row, doubling, or
/// `None` once they have gone on for `RIDE_OUT`.
fn retry_delay(retries: u32, failing: Duration) -> Option<Duration> {
    (failing < RIDE_OUT).then(|| (RETRY_DELAY * 2u32.saturating_pow(retries)).min(MAX_RETRY_DELAY))
}

fn check_host(url: &str) -> Result<(), String> {
    if crate::http::allowed(url) {
        return Ok(());
    }
    log::warn!("refused a stream from a host YouTube does not use");
    Err("unexpected stream host".into())
}

/// Whether the connection failed, rather than googlevideo answering with
/// an error or bytes that don't fit.
fn is_connection(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<io::Error>()
        || error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.status().is_none())
}

/// Whether googlevideo refused the URL itself rather than one request.
fn is_refused(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        == Some(reqwest::StatusCode::FORBIDDEN)
}

/// A fresh URL for the file `old` serves. It must be the same encode, as
/// the bytes already downloaded have to line up with the rest.
fn refreshed(refresh: &mut Refresh, old: &Source, again: bool) -> Result<Source, String> {
    let new = refresh(again)?;
    check_host(&new.url)?;
    if new.size != old.size || !same_file(&old.url, &new.url) {
        return Err("YouTube served a different file".into());
    }
    Ok(new)
}

/// Whether two googlevideo URLs serve the same file: the same format, of
/// the same length, from the same encode.
fn same_file(a: &str, b: &str) -> bool {
    query_param(a, "itag").is_some()
        && ["itag", "clen", "lmt"]
            .iter()
            .all(|name| query_param(a, name) == query_param(b, name))
}

fn query_param<'a>(url: &'a str, name: &str) -> Option<&'a str> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

pub struct StreamReader {
    download: Arc<Download>,
    pos: u64,
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.download.state.lock().unwrap();
        // The download follows the reader, and a window waits on it moving on.
        if state.pos / CHUNK != self.pos / CHUNK {
            self.download.changed.notify_all();
        }
        state.pos = self.pos;
        loop {
            if state.cancelled {
                // Not `Interrupted`, which Symphonia's exact reads retry
                // for ever, hanging the engine on a skip.
                return Err(io::Error::other("download cancelled"));
            }
            if let Some(file) = self.download.file.get() {
                drop(state);
                // Positional, as seeking moves only `pos`.
                let n = file.seek_read(buf, self.pos)?;
                self.pos += n as u64;
                return Ok(n);
            }
            let block = state.blocks.get((self.pos / CHUNK) as usize);
            let held = block.and_then(|b| b.get((self.pos % CHUNK) as usize..));
            if let Some(held) = held.filter(|h| !h.is_empty()) {
                let n = buf.len().min(held.len());
                buf[..n].copy_from_slice(&held[..n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if state.total > 0 && self.pos >= state.total {
                return Ok(0);
            }
            if let Some(e) = &state.error {
                return Err(io::Error::other(e.clone()));
            }
            state = self.download.changed.wait(state).unwrap();
        }
    }
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        // A download whose track is dropped is not always cancelled, so a
        // window waiting on its reader would otherwise wait for ever.
        if let Ok(mut state) = self.download.state.lock() {
            state.readers -= 1;
            state.abandoned = state.readers == 0;
        }
        self.download.changed.notify_all();
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let total = self.download.state.lock().unwrap().total;
        let target = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => self.pos.checked_add_signed(n),
            SeekFrom::End(n) => total.checked_add_signed(n),
        };
        self.pos = target
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek out of range"))?;
        Ok(self.pos)
    }
}

impl MediaSource for StreamReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        let total = self.download.state.lock().unwrap().total;
        (total > 0).then_some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://rr1.googlevideo.com/videoplayback?expire=1&itag=251&clen=4171552&lmt=1700000000000000&n=a";

    #[test]
    fn a_missing_file_fails_to_read() {
        let path = std::env::temp_dir()
            .join("lily-no-such-folder")
            .join("a.flac");
        let mut reader = Download::open(path).reader();
        let e = reader.read(&mut [0; 4]).unwrap_err();
        assert!(e.to_string().contains("couldn't open"), "{e}");
    }

    #[test]
    fn resumes_only_on_the_same_file() {
        let fresh = "https://rr4.googlevideo.com/videoplayback?itag=251&n=b&lmt=1700000000000000&clen=4171552";
        assert!(same_file(URL, fresh), "another host and order, same file");
        assert!(!same_file(URL, &fresh.replace("itag=251", "itag=250")));
        assert!(!same_file(
            URL,
            &fresh.replace("clen=4171552", "clen=4171553")
        ));
        assert!(
            !same_file(URL, &fresh.replace("lmt=1700000000000000", "lmt=1")),
            "re-encoded"
        );
        assert!(
            !same_file("https://a/251", "https://a/251"),
            "no format to go by"
        );
    }

    fn source(url: &str, size: u64) -> Source {
        Source {
            url: url.into(),
            size,
            headers: Vec::new(),
            head: Vec::new(),
        }
    }

    fn refresh_to(url: &'static str, size: u64) -> Refresh {
        Box::new(move |_| Ok(source(url, size)))
    }

    #[test]
    fn takes_a_fresh_url_only_for_the_same_file_and_host() {
        let old = source(URL, 4171552);
        let fresh =
            "https://rr4.googlevideo.com/videoplayback?itag=251&clen=4171552&lmt=1700000000000000";
        assert!(refreshed(&mut refresh_to(fresh, 4171552), &old, false).is_ok());
        assert!(
            refreshed(&mut refresh_to(fresh, 4171553), &old, false).is_err(),
            "another length"
        );
        let elsewhere =
            "https://example.com/videoplayback?itag=251&clen=4171552&lmt=1700000000000000";
        assert_eq!(
            refreshed(&mut refresh_to(elsewhere, 4171552), &old, false).err(),
            Some("unexpected stream host".into())
        );
        let mut failing: Refresh = Box::new(|_| Err("offline".into()));
        assert_eq!(
            refreshed(&mut failing, &old, false).err(),
            Some("offline".into())
        );
    }

    #[test]
    fn tells_refusals_and_lost_connections_from_other_errors() {
        let answer = |status: u16| {
            let response = http::Response::builder().status(status).body("").unwrap();
            reqwest::Response::from(response)
                .error_for_status()
                .map_err(reqwest::Error::without_url)
                .unwrap_err()
        };
        assert!(is_refused(&answer(403)));
        assert!(!is_refused(&answer(404)), "another status is retried");
        assert!(!is_refused(&answer(503)));
        let other: Box<dyn std::error::Error> = "the server sent no bytes".into();
        assert!(!is_refused(&*other), "not an HTTP error");
        assert!(
            !is_connection(&answer(503)),
            "an answer, not a lost connection"
        );
        assert!(!is_connection(&*other));
        assert!(is_connection(&io::Error::from(
            io::ErrorKind::ConnectionReset
        )));
    }

    #[test]
    fn a_cancelled_read_fails_for_good() {
        let download = Arc::new(Download::default());
        download.state.lock().unwrap().put(0, &[1; 8]);
        download.cancel();
        let kind = download.reader().read(&mut [0; 4]).unwrap_err().kind();
        assert_ne!(kind, io::ErrorKind::Interrupted, "Symphonia would retry");
        assert_ne!(kind, io::ErrorKind::UnexpectedEof, "not the stream's end");
        assert_eq!(download.state.lock().unwrap().blocks.capacity(), 0);
    }

    #[test]
    fn plays_again_only_a_whole_download_of_the_same_url() {
        let download = Arc::new(Download {
            url: URL.into(),
            ..Download::default()
        });
        download.state.lock().unwrap().put(0, &[1, 2, 3]);
        assert!(!download.whole(URL), "still downloading");
        download.state.lock().unwrap().done = true;
        assert!(download.whole(URL));
        assert!(
            !download.whole(&URL.replace("n=a", "n=b")),
            "another stream"
        );
        download.cancel();
        assert!(!download.whole(URL), "cancelled, so emptied");
    }

    const MB: u64 = CHUNK;

    fn sized(total: u64) -> State {
        State {
            total,
            ..State::default()
        }
    }

    fn fill(state: &mut State, block: u64) {
        let len = (state.total - block * MB).min(MB) as usize;
        let at = block * MB + state.held(block as usize) as u64;
        let rest = len - state.held(block as usize);
        state.put(at, &vec![block as u8; rest]);
    }

    #[test]
    fn fetches_a_seek_first_then_fills_the_gaps() {
        let mut state = sized(5 * MB + 100);
        state.put(0, &[0; 64 * 1024]);
        assert_eq!(state.next_range(), Some(64 * 1024), "on from the head");
        state.pos = 3 * MB + 10;
        assert_eq!(state.next_range(), Some(3 * MB), "the seek's block first");
        state.put(3 * MB, &[3; 1000]);
        assert_eq!(
            state.next_range(),
            Some(3 * MB + 1000),
            "then the rest of it"
        );
        fill(&mut state, 3);
        fill(&mut state, 4);
        assert_eq!(state.next_range(), Some(5 * MB), "on to the end");
        fill(&mut state, 5);
        assert_eq!(state.next_range(), Some(64 * 1024), "then the gap before");
        assert_eq!(state.run_end(state.pos), state.total);
        assert_eq!(state.run_end(MB), MB, "nothing held there");
        for block in 0..3 {
            fill(&mut state, block);
        }
        assert_eq!(state.next_range(), None);
        assert_eq!(state.run_end(0), state.total, "all of it");
    }

    #[test]
    fn a_stream_of_unknown_length_runs_on_across_blocks() {
        let mut state = State::default();
        state.put(0, &vec![1; MB as usize + 5]);
        assert_eq!(state.blocks.len(), 2);
        assert_eq!(state.next_range(), Some(MB + 5));
    }

    #[test]
    fn reads_a_block_held_past_a_gap() {
        let download = Arc::new(Download::sized(4 * MB));
        fill(&mut download.state.lock().unwrap(), 2);
        let next = || download.state.lock().unwrap().next_range();
        let mut reader = download.reader();
        reader.seek(SeekFrom::Start(2 * MB + 1)).unwrap();
        assert_eq!(next(), Some(0), "a seek alone moves nothing");
        let mut buf = [0; 4];
        assert_eq!(reader.read(&mut buf).unwrap(), 4);
        assert_eq!(buf, [2; 4]);
        assert_eq!(next(), Some(3 * MB), "on from where it read");
        reader.seek(SeekFrom::Start(4 * MB)).unwrap();
        assert_eq!(reader.read(&mut buf).unwrap(), 0, "the end, gaps or not");
    }

    #[test]
    fn a_window_holds_only_blocks_around_the_reader() {
        let mut state = State {
            window: true,
            ..sized(100 * MB)
        };
        for block in 0..40 {
            fill(&mut state, block);
        }
        state.pos = 30 * MB;
        state.evict();
        let held: Vec<_> = (0..100).filter(|&b| state.held(b) > 0).collect();
        assert_eq!(held, (22..40).collect::<Vec<_>>());
        assert_eq!(
            state.next_range(),
            Some(40 * MB),
            "ahead, not the gap behind"
        );
        for block in 40..54 {
            fill(&mut state, block);
        }
        assert_eq!(state.next_range(), None, "far enough ahead");
        state.pos = 5 * MB;
        state.evict();
        assert!(
            state.blocks.iter().skip(29).all(Vec::is_empty),
            "dropped ahead too"
        );
        assert_eq!(
            state.next_range(),
            Some(5 * MB),
            "a seek back fetches again"
        );
    }

    #[test]
    fn a_window_stops_once_nothing_reads_it() {
        let download = Arc::new(Download::sized(100 * MB));
        {
            let mut state = download.state.lock().unwrap();
            state.window = true;
            for block in 0..KEEP_AHEAD as u64 {
                fill(&mut state, block);
            }
        }
        let reader = download.reader();
        let (tx, stopped) = std::sync::mpsc::channel();
        let worker = Arc::clone(&download);
        thread::spawn(move || {
            worker.run(source(URL, 100 * MB), refresh_to(URL, 100 * MB));
            let _ = tx.send(());
        });
        let wait = Duration::from_millis(200);
        assert!(stopped.recv_timeout(wait).is_err(), "waits on the reader");
        drop(reader);
        assert!(stopped.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(!download.whole(URL), "never done");
    }

    #[test]
    fn seeks_within_range_only() {
        let mut reader = Arc::new(Download::default()).reader();
        assert_eq!(reader.seek(SeekFrom::Current(1)).unwrap(), 1);
        assert!(reader.seek(SeekFrom::Current(-2)).is_err(), "before start");
        assert!(reader.seek(SeekFrom::End(-1)).is_err(), "no length yet");
        assert_eq!(reader.seek(SeekFrom::Current(i64::MAX)).unwrap(), 1 << 63);
        assert!(reader.seek(SeekFrom::Current(i64::MAX)).is_ok());
        assert!(
            reader.seek(SeekFrom::Current(i64::MAX)).is_err(),
            "overflow"
        );
    }

    #[test]
    fn retries_a_dropped_connection_until_it_has_been_gone_a_while() {
        let secs = Duration::from_secs;
        assert_eq!(retry_delay(0, Duration::ZERO), Some(RETRY_DELAY));
        assert_eq!(retry_delay(1, secs(1)), Some(RETRY_DELAY * 2));
        assert_eq!(retry_delay(40, secs(30)), Some(MAX_RETRY_DELAY), "capped");
        assert_eq!(retry_delay(0, RIDE_OUT), None, "given up");
    }

    #[test]
    fn reads_whole_query_parameters() {
        let url = "https://a/v?xitag=1&itag=251&itag2=3";
        assert_eq!(query_param(url, "itag"), Some("251"));
        assert_eq!(query_param(url, "clen"), None);
        assert_eq!(query_param("https://a/v", "itag"), None);
    }
}
