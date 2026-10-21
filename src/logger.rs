use log::{LevelFilter, Log, Metadata, Record};

const MAX_LEVEL: LevelFilter = if cfg!(debug_assertions) {
    LevelFilter::Debug
} else {
    LevelFilter::Warn
};

/// Writes this crate's records to stderr; dependency logs are dropped.
struct StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= MAX_LEVEL && metadata.target().starts_with(env!("CARGO_CRATE_NAME"))
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

pub fn init() {
    static LOGGER: StderrLogger = StderrLogger;
    log::set_logger(&LOGGER).expect("logger is initialised once");
    log::set_max_level(MAX_LEVEL);
}
