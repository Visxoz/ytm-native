//! Downloads a stream into memory in the background and serves it to the
//! demuxer as a seekable file, blocking reads until their bytes arrive.

use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread,
    time::Duration,
};

use symphonia::core::io::MediaSource;

/// googlevideo throttles large single requests, so the file is fetched in
/// ranges no bigger than this.
const CHUNK: u64 = 1024 * 1024;
const READ_BUF: usize = 64 * 1024;
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_millis(500);

fn http() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("HTTP client builds")
    })
}

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
    /// Starts fetching `url`, whose length YouTube reports up front.
    pub fn start(url: String, total: u64, headers: Vec<(String, String)>) -> Arc<Self> {
        let download = Arc::new(Self::default());
        download.state.lock().unwrap().total = total;
        let worker = Arc::clone(&download);
        thread::Builder::new()
            .name("stream-download".into())
            .spawn(move || worker.run(&url, &headers))
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

    fn run(&self, url: &str, headers: &[(String, String)]) {
        let mut retries = 0;
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
            match self.fetch(url, headers, offset, total) {
                Ok(true) => break,
                Ok(false) => retries = 0,
                Err(e) if retries < MAX_RETRIES => {
                    retries += 1;
                    log::debug!("download chunk failed, retrying ({retries}): {e}");
                    thread::sleep(RETRY_DELAY * retries);
                }
                Err(e) => {
                    log::warn!("download failed: {e}");
                    self.state.lock().unwrap().error = Some(e.to_string());
                    self.changed.notify_all();
                    return;
                }
            }
        }
        self.state.lock().unwrap().done = true;
        self.changed.notify_all();
    }

    /// Fetches one range starting at `offset`. Returns `true` once the whole
    /// file has arrived.
    fn fetch(
        &self,
        url: &str,
        headers: &[(String, String)],
        offset: u64,
        total: u64,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let mut request = http().get(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if total > 0 {
            let end = (offset + CHUNK).min(total) - 1;
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-{end}"));
        }
        let mut response = request.send()?.error_for_status()?;
        if total == 0 {
            let length = response.content_length().unwrap_or(0);
            self.state.lock().unwrap().total = length;
        }

        let mut buf = vec![0; READ_BUF];
        loop {
            let n = response.read(&mut buf)?;
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
        Ok(total == 0 || state.data.len() as u64 >= state.total)
    }
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
