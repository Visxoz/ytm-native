use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    panic::PanicHookInfo,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError, TryLockError},
    time::Duration,
};

use log::{LevelFilter, Log, Metadata, Record};

const DEFAULT_LEVEL: LevelFilter = if cfg!(debug_assertions) {
    LevelFilter::Debug
} else {
    LevelFilter::Warn
};

/// The Tokio runtime's threads. A panic there fails only its request, as the
/// task's `JoinHandle` reports it; anywhere else it ends the app.
pub const REQUEST_THREAD: &str = "lily-request";

/// Past this the log starts afresh, keeping the one before as `lily.old.log`.
const MAX_FILE_BYTES: u64 = 1 << 20;

struct Sink {
    file: File,
    written: u64,
    path: PathBuf,
}

/// The release build's log file, once the data folder is settled.
static FILE: Mutex<Option<Sink>> = Mutex::new(None);

/// Writes this crate's records to stderr, and in release builds to the log
/// file; dependency logs are dropped.
struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
            && metadata.target().starts_with(env!("CARGO_CRATE_NAME"))
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let line = format!("[{} {}] {}", record.level(), record.target(), record.args());
            eprintln!("{line}");
            to_file(&line);
        }
    }

    fn flush(&self) {}
}

/// `YTM_LOG=info` (or `debug`) raises the level, for measuring release builds.
pub fn init() {
    static LOGGER: Logger = Logger;
    log::set_logger(&LOGGER).expect("logger is initialised once");
    let level = std::env::var("YTM_LOG")
        .ok()
        .and_then(|l| l.parse().ok())
        .unwrap_or(DEFAULT_LEVEL);
    log::set_max_level(level);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        on_panic(info);
        default(info);
        if std::thread::current().name() != Some(REQUEST_THREAD) {
            // As `panic = "abort"` did: the UI's and the audio threads' state
            // isn't left half-changed, nor a read waiting on a dead download.
            std::process::abort();
        }
    }));
}

fn on_panic(info: &PanicHookInfo) {
    let thread = std::thread::current();
    let message = info.payload_as_str().unwrap_or("no message");
    let at = info
        .location()
        .map(|l| format!("{}:{}", l.file(), l.line()))
        .unwrap_or_default();
    let line = format!(
        "[PANIC] thread '{}' at {at}: {message}",
        thread.name().unwrap_or("unnamed")
    );
    // Tried rather than waited on, as a panic while this thread writes holds
    // the lock already; another thread's write takes well under this.
    let line = stamp(&line);
    for _ in 0..100 {
        match FILE.try_lock() {
            Ok(sink) => return write(sink, &line),
            Err(TryLockError::Poisoned(e)) => return write(e.into_inner(), &line),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// Starts the release build's log file in the data folder; called once this
/// copy holds the single-instance lock, so a second launch never rotates it.
pub fn open_file() {
    if cfg!(debug_assertions) {
        return;
    }
    match open(
        &crate::settings::data_dir().join("lily.log"),
        MAX_FILE_BYTES,
    ) {
        Ok(sink) => {
            *FILE.lock().unwrap_or_else(PoisonError::into_inner) = Some(sink);
            to_file(concat!("Lily ", env!("CARGO_PKG_VERSION"), " started"));
        }
        Err(e) => log::warn!("could not open the log file: {e}"),
    }
}

/// Opens `path` to append to, first moving it aside once `limit` long.
fn open(path: &Path, limit: u64) -> io::Result<Sink> {
    let mut options = OpenOptions::new();
    options.create(true);
    let full = fs::metadata(path).is_ok_and(|m| m.len() >= limit);
    if full && fs::rename(path, path.with_extension("old.log")).is_err() {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    let file = options.open(path)?;
    let written = file.metadata()?.len();
    Ok(Sink {
        file,
        written,
        path: path.to_owned(),
    })
}

fn to_file(line: &str) {
    // Debug builds have no file, so nothing to format for it.
    if cfg!(debug_assertions) {
        return;
    }
    let line = stamp(line);
    // Never unwrapped: a poisoned lock would panic every later record.
    write(FILE.lock().unwrap_or_else(PoisonError::into_inner), &line);
}

/// The line as the file holds it, made before taking the lock.
fn stamp(line: &str) -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!(
        "{} {:02}:{:02}:{:02} {}\r\n",
        now.date(),
        now.hour(),
        now.minute(),
        now.second(),
        redact(line)
    )
}

fn write(mut sink: MutexGuard<Option<Sink>>, line: &str) {
    let Some(open_sink) = sink.as_mut() else {
        return;
    };
    // Unbuffered, so a line written just before an abort is kept.
    if open_sink.file.write_all(line.as_bytes()).is_ok() {
        open_sink.written += line.len() as u64;
    }
    if open_sink.written >= MAX_FILE_BYTES {
        let path = open_sink.path.clone();
        // Closed before it is moved aside.
        *sink = None;
        *sink = open(&path, MAX_FILE_BYTES).ok();
    }
}

/// Cuts every URL down to its host, as paths and queries carry stream
/// signatures and IDs, and a proxy's address its password.
fn redact(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + 3);
        out.push_str(before);
        // Only where a URL can't go on, so a bracket in its query leaks nothing.
        let end = after
            .find(|c: char| c.is_whitespace() || c == '"')
            .unwrap_or(after.len());
        let url = &after[..end];
        let authority = &url[..url.find(['/', '?', '#']).unwrap_or(url.len())];
        let host = authority.rsplit('@').next().unwrap_or_default();
        out.push_str(host);
        if host.len() < url.len() {
            out.push_str("/…");
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_keeps_only_hosts() {
        assert_eq!(
            redact(
                "error sending request for url (https://rr1.googlevideo.com/videoplayback?sig=x&ip=1)"
            ),
            "error sending request for url (https://rr1.googlevideo.com/…"
        );
        assert_eq!(
            redact("proxy socks5://user:secret@10.0.0.1:1080 refused"),
            "proxy socks5://10.0.0.1:1080/… refused"
        );
        assert_eq!(
            redact("\"https://host/a?q=(x)&sig=secret\" failed"),
            "\"https://host/…\" failed"
        );
        assert_eq!(
            redact("https://music.youtube.com"),
            "https://music.youtube.com"
        );
        assert_eq!(redact("no links here"), "no links here");
    }

    #[test]
    fn a_full_log_is_moved_aside() {
        let dir = std::env::temp_dir().join(format!("lily-log-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lily.log");
        fs::write(&path, "0123456789").unwrap();

        let sink = open(&path, 100).unwrap();
        assert_eq!(sink.written, 10, "under the limit it is appended to");
        drop(sink);

        let sink = open(&path, 10).unwrap();
        assert_eq!(sink.written, 0);
        drop(sink);
        assert_eq!(
            fs::read_to_string(dir.join("lily.old.log")).unwrap(),
            "0123456789"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
