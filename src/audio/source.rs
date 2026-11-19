//! Downloads a stream into memory in the background and serves it to the
//! demuxer as a seekable file, blocking reads until their bytes arrive. A
//! file on disk is served in place.

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
/// ranges no bigger than this.
const CHUNK: u64 = 1024 * 1024;
const READ_BUF: usize = 64 * 1024;
/// Caps the up-front allocation should a reported size be wrong; enough for a long mix.
const MAX_RESERVE: u64 = 1024 * 1024 * 1024;
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
}

/// Asks YouTube again for the stream, as googlevideo sometimes serves a URL's
/// first megabyte and refuses the rest. Told whether the last fresh URL was
/// refused too. Runs on the download thread, so may block.
pub type Refresh = Box<dyn FnMut(bool) -> Result<Source, String> + Send>;

#[derive(Default)]
struct State {
    data: Vec<u8>,
    total: u64,
    done: bool,
    cancelled: bool,
    error: Option<String>,
    /// The error is the connection's, which outlasted `RIDE_OUT`.
    dropped: bool,
}

#[derive(Default)]
pub struct Download {
    state: Mutex<State>,
    changed: Condvar,
    /// A file read in place, set before `done`.
    file: OnceLock<File>,
}

impl Download {
    fn sized(size: u64) -> Self {
        let download = Self::default();
        {
            let mut state = download.state.lock().unwrap();
            state.total = size;
            // Sized once, as growing by doubling can hold twice the file.
            // Tried, so a wrong size too big to set aside grows as it downloads.
            let _ = state.data.try_reserve_exact(size.min(MAX_RESERVE) as usize);
        }
        download
    }

    pub fn start(source: Source, refresh: Refresh) -> Arc<Self> {
        let download = Arc::new(Self::sized(source.size));
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
        state.error.map_or(Ok(state.data), Err)
    }

    /// Stops the download and fails any blocked read.
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.cancelled = true;
        // Nothing reads a cancelled download, though a slot or a thread
        // waiting on the network may hold it a while yet.
        state.data = Vec::new();
        drop(state);
        self.changed.notify_all();
    }

    pub fn progress(&self) -> f32 {
        let state = self.state.lock().unwrap();
        if state.done {
            1.0
        } else if state.total == 0 {
            0.0
        } else {
            state.data.len() as f32 / state.total as f32
        }
    }

    /// Whether it gave up on a connection that stopped answering, rather
    /// than on a refusal or a bad file.
    pub fn dropped(&self) -> bool {
        self.state.lock().unwrap().dropped
    }

    pub fn reader(self: &Arc<Self>) -> StreamReader {
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
                let state = self.state.lock().unwrap();
                if state.cancelled {
                    return;
                }
                (state.data.len() as u64, state.total)
            };
            if total > 0 && offset >= total {
                break;
            }
            let error = match self.fetch(&source, offset, total, &mut buf) {
                Ok(true) => break,
                Ok(false) => {
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
                if state.data.len() as u64 > offset {
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
        self.state.lock().unwrap().done = true;
        self.changed.notify_all();
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

    /// Fetches one range starting at `offset`, reading through `buf`.
    /// Returns `true` once the whole file has arrived.
    fn fetch(
        &self,
        source: &Source,
        offset: u64,
        total: u64,
        buf: &mut [u8],
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let mut request = crate::http::blocking_client().get(&source.url);
        for (name, value) in &source.headers {
            request = request.header(name, value);
        }
        if total > 0 {
            let end = (offset + CHUNK).min(total) - 1;
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-{end}"));
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
            let length = response.content_length().unwrap_or(0);
            let mut state = self.state.lock().unwrap();
            state.total = length;
            let _ = state
                .data
                .try_reserve_exact(length.min(MAX_RESERVE) as usize);
        }

        loop {
            let n = response.read(buf)?;
            if n == 0 {
                break;
            }
            let mut state = self.state.lock().unwrap();
            if state.cancelled {
                return Ok(true);
            }
            state.data.extend_from_slice(&buf[..n]);
            drop(state);
            self.changed.notify_all();
        }
        let state = self.state.lock().unwrap();
        let len = state.data.len() as u64;
        // Only this thread adds to the data, so nothing past where the range
        // started means an empty answer, which would be asked again forever.
        if len == offset {
            return Err("the server sent no bytes".into());
        }
        Ok(total == 0 || len >= state.total)
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
            let available = state.data.len() as u64;
            if self.pos < available {
                let start = self.pos as usize;
                let n = buf.len().min(state.data.len() - start);
                buf[..n].copy_from_slice(&state.data[start..start + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if state.done || (state.total > 0 && self.pos >= state.total) {
                return Ok(0);
            }
            if let Some(e) = &state.error {
                return Err(io::Error::other(e.clone()));
            }
            state = self.download.changed.wait(state).unwrap();
        }
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
        download.state.lock().unwrap().data = vec![1; 8];
        download.cancel();
        let kind = download.reader().read(&mut [0; 4]).unwrap_err().kind();
        assert_ne!(kind, io::ErrorKind::Interrupted, "Symphonia would retry");
        assert_ne!(kind, io::ErrorKind::UnexpectedEof, "not the stream's end");
        assert_eq!(download.state.lock().unwrap().data.capacity(), 0);
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
