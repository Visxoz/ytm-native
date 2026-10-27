//! Downloads a stream into memory in the background and serves it to the
//! demuxer as a seekable file, blocking reads until their bytes arrive.

use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Duration,
};

use symphonia::core::io::MediaSource;

/// googlevideo throttles large single requests, so the file is fetched in
/// ranges no bigger than this.
const CHUNK: u64 = 1024 * 1024;
const READ_BUF: usize = 64 * 1024;
/// Upper bound on the up-front allocation, should a reported size be wrong.
const MAX_RESERVE: u64 = 64 * 1024 * 1024;
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Fresh URLs asked for in a row without a whole range arriving from them.
const MAX_REFRESHES: u32 = 2;

/// Where a stream's bytes come from.
pub struct Source {
    pub url: String,
    /// Length YouTube reports up front, or 0 if it did not.
    pub size: u64,
    pub headers: Vec<(String, String)>,
}

/// Asks YouTube again for the stream being downloaded. It sometimes hands
/// out a URL that serves the first megabyte or so and refuses the rest,
/// while a fresh one for the same file serves it whole. Runs on the
/// download's own thread, so it may block.
pub type Refresh = Box<dyn FnMut() -> Result<Source, String> + Send>;

#[derive(Default)]
struct State {
    data: Vec<u8>,
    total: u64,
    done: bool,
    cancelled: bool,
    error: Option<String>,
}

#[derive(Default)]
pub struct Download {
    state: Mutex<State>,
    changed: Condvar,
}

impl Download {
    /// Starts fetching `source`, asking `refresh` for a fresh URL should
    /// googlevideo refuse it.
    pub fn start(source: Source, refresh: Refresh) -> Arc<Self> {
        let download = Arc::new(Self::default());
        {
            let mut state = download.state.lock().unwrap();
            state.total = source.size;
            // Sized once, as growing by doubling can hold twice the file.
            state.data = Vec::with_capacity(source.size.min(MAX_RESERVE) as usize);
        }
        let worker = Arc::clone(&download);
        thread::Builder::new()
            .name("stream-download".into())
            .spawn(move || worker.run(source, refresh))
            .expect("download thread spawns");
        download
    }

    /// Stops the download and fails any blocked read.
    pub fn cancel(&self) {
        self.state.lock().unwrap().cancelled = true;
        self.changed.notify_all();
    }

    /// Fraction of the file downloaded so far.
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
        let (mut retries, mut refreshes) = (0, 0);
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
                    refreshes = 0;
                    continue;
                }
                Err(e) => e,
            };
            // Nobody reads a cancelled download, so it neither waits nor asks.
            if self.state.lock().unwrap().cancelled {
                return;
            }
            let refused = is_refused(&*error);
            if refused && refreshes < MAX_REFRESHES {
                // Asking the same URL again cannot help, but a fresh one
                // carries on from the same byte.
                refreshes += 1;
                match refreshed(&mut refresh, &source) {
                    Ok(new) => {
                        source = new;
                        continue;
                    }
                    Err(e) => log::warn!("no fresh URL for a refused download: {e}"),
                }
            } else if !refused && retries < MAX_RETRIES {
                retries += 1;
                log::debug!("download chunk failed, retrying ({retries}): {error}");
                thread::sleep(RETRY_DELAY * retries);
                continue;
            }
            log::warn!("download failed: {error}");
            return self.fail(error.to_string());
        }
        self.state.lock().unwrap().done = true;
        self.changed.notify_all();
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
        if total == 0 {
            let length = response.content_length().unwrap_or(0);
            let mut state = self.state.lock().unwrap();
            state.total = length;
            state.data.reserve(length.min(MAX_RESERVE) as usize);
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

/// Refuses a stream from a host YouTube does not use.
fn check_host(url: &str) -> Result<(), String> {
    if crate::http::allowed(url) {
        return Ok(());
    }
    log::warn!("refused a stream from a host YouTube does not use");
    Err("unexpected stream host".into())
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
fn refreshed(refresh: &mut Refresh, old: &Source) -> Result<Source, String> {
    let new = refresh()?;
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
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "download cancelled",
                ));
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
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(n) => self.pos as i64 + n,
            SeekFrom::End(n) => total as i64 + n,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = target as u64;
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
        Box::new(move || Ok(source(url, size)))
    }

    #[test]
    fn takes_a_fresh_url_only_for_the_same_file_and_host() {
        let old = source(URL, 4171552);
        let fresh =
            "https://rr4.googlevideo.com/videoplayback?itag=251&clen=4171552&lmt=1700000000000000";
        assert!(refreshed(&mut refresh_to(fresh, 4171552), &old).is_ok());
        assert!(
            refreshed(&mut refresh_to(fresh, 4171553), &old).is_err(),
            "another length"
        );
        let elsewhere =
            "https://example.com/videoplayback?itag=251&clen=4171552&lmt=1700000000000000";
        assert_eq!(
            refreshed(&mut refresh_to(elsewhere, 4171552), &old).err(),
            Some("unexpected stream host".into())
        );
        let mut failing: Refresh = Box::new(|| Err("offline".into()));
        assert_eq!(refreshed(&mut failing, &old).err(), Some("offline".into()));
    }

    #[test]
    fn only_a_forbidden_answer_asks_for_a_fresh_url() {
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
    }

    #[test]
    fn reads_whole_query_parameters() {
        let url = "https://a/v?xitag=1&itag=251&itag2=3";
        assert_eq!(query_param(url, "itag"), Some("251"));
        assert_eq!(query_param(url, "clen"), None);
        assert_eq!(query_param("https://a/v", "itag"), None);
    }
}
