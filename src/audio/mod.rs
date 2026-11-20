//! Audio engine: one thread decodes into a sample queue the device drains.
//! The queued track's samples follow in the same queue, mixed over the held
//! tail for a crossfade, and the app hears of the change only once the
//! device callback reaches them.

mod decode;
mod engine;
pub mod eq;
mod opus;
mod output;
mod source;

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
        mpsc::{self, Sender},
    },
    thread,
};

use engine::Engine;
use output::Output;
use source::Download;
pub use source::{Refresh, Source};

pub const MIN_SPEED: f32 = 0.5;
pub const MAX_SPEED: f32 = 2.0;

/// The most silence cut from either side of a crossfade, so a quiet
/// passage or a hidden track is not mistaken for the end.
pub const SILENCE_TRIM_SECS: f64 = 4.0;

/// A stream to play: where its bytes come from and what the engine opens
/// them with.
pub struct StreamSpec {
    pub input: Input,
    pub opening: Opening,
}

pub enum Input {
    /// Downloaded, with how to ask for it again should googlevideo refuse
    /// the URL.
    Stream(Source, Refresh),
    File(PathBuf),
}

pub struct Opening {
    pub mime: String,
    /// Integrated loudness YouTube measured, used to even out volume.
    pub loudness_db: Option<f32>,
    pub start_at: f64,
}

/// The output devices plugged in, as cpal ID and name. Slow, and cpal sets up
/// COM on the calling thread, so best called on one of its own.
pub fn output_devices() -> Vec<(String, String)> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let Ok(devices) = cpal::default_host().output_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|d| {
            Some((
                d.id().ok()?.to_string(),
                d.description().ok()?.name().to_owned(),
            ))
        })
        .collect()
}

/// The whole stream, fetched on this thread, which may block, for keeping.
pub fn fetch(source: Source, refresh: Refresh) -> Result<Vec<u8>, String> {
    Download::fetch_all(source, refresh)
}

#[derive(Debug)]
pub enum PlayerEvent {
    /// The track has started producing sound.
    Started,
    /// The queued track has become audible, with no gap.
    Advanced,
    /// The queued track was dropped, as a seek kept the current one going.
    NextDropped,
    /// The queued track's stream failed to open, so it is not worth
    /// reusing; the track loads afresh once the current one ends. `dropped`
    /// as for `Dropped`.
    NextFailed {
        dropped: bool,
    },
    /// The track played to the end, with nothing queued.
    Ended,
    Error(String),
    /// The track's download gave up on a connection that stopped answering.
    Dropped(String),
}

enum Command {
    Load(Opening, Arc<Download>),
    Queue(Opening, Arc<Download>, f64),
    ClearNext,
    Seek(f64),
    Stop,
    /// Pauses or resumes the device to match `Shared::paused`.
    Pause,
    /// Opens the device again, as another or a new speed was chosen.
    Reopen,
}

/// Empties a download slot and cancels what it held, once the slot's lock
/// is released, unless `other` holds it too, as a song queued to play again.
fn cancel(slot: &Mutex<Option<Arc<Download>>>, other: &Mutex<Option<Arc<Download>>>) {
    let taken = slot.lock().unwrap().take();
    if let Some(download) = taken
        && !other
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|d| Arc::ptr_eq(d, &download))
    {
        download.cancel();
    }
}

/// The gain that brings a track to YouTube's reference loudness, before the
/// level and the cap; a track with none measured is taken as at it.
fn loudness_gain(loudness_db: Option<f32>) -> f32 {
    loudness_db.map_or(1.0, |db| 10f32.powf(-db / 20.0))
}

/// The most a quiet track is turned up, +3 dB, as its peaks are unknown and
/// quiet tracks are often the dynamic ones; the limiter catches the rest.
const MAX_BOOST: f32 = 1.412_537_5;

/// How tracks are evened out: Normal to YouTube's reference loudness, Quiet
/// 6 dB under it, as Spotify's Quiet sits under its Normal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Normalise {
    Off,
    Quiet,
    Normal,
}

impl Normalise {
    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            0 => Normalise::Off,
            1 => Normalise::Quiet,
            _ => Normalise::Normal,
        }
    }

    /// The gain played for a track whose `loudness_gain` is `gain`. The cap
    /// comes after the level, so Quiet turns up as far as Normal does.
    fn apply(self, gain: f32) -> f32 {
        match self {
            Normalise::Off => 1.0,
            Normalise::Quiet => (gain * 0.5).min(MAX_BOOST),
            Normalise::Normal => gain.min(MAX_BOOST),
        }
    }
}

struct Shared {
    output: Mutex<Output>,
    paused: AtomicBool,
    /// Set by the device's error callback once its stream needs reopening.
    device_lost: AtomicBool,
    /// The volume and the loudness correction, as f32 bits.
    volume: AtomicU32,
    /// A `Normalise`, as its index.
    normalise: AtomicU8,
    loudness_gain: AtomicU32,
    /// The audible track's download.
    download: Mutex<Option<Arc<Download>>>,
    /// The queued track's download.
    next_download: Mutex<Option<Arc<Download>>>,
    /// The chosen output device's cpal ID, or `None` for the default.
    device: Mutex<Option<String>>,
    /// The playback speed, as f32 bits.
    speed: AtomicU32,
}

impl Shared {
    fn set_loudness(&self, gain: f32) {
        self.loudness_gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    fn cancel_current(&self) {
        cancel(&self.download, &self.next_download);
    }

    fn cancel_next(&self) {
        cancel(&self.next_download, &self.download);
    }

    fn correction(&self, gain: f32) -> f32 {
        Normalise::from_index(self.normalise.load(Ordering::Relaxed).into()).apply(gain)
    }

    fn loudness(&self) -> f32 {
        self.correction(f32::from_bits(self.loudness_gain.load(Ordering::Relaxed)))
    }

    fn gain(&self) -> f32 {
        let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
        // Squared so the slider feels even to the ear.
        volume * volume * self.loudness()
    }
}

#[derive(Clone)]
pub struct Player {
    tx: Sender<Command>,
    shared: Arc<Shared>,
}

impl Player {
    pub fn spawn(on_event: impl Fn(PlayerEvent) + Send + 'static) -> Self {
        let shared = Arc::new(Shared {
            output: Mutex::new(Output {
                channels: 2,
                next_loudness: 1.0,
                ..Default::default()
            }),
            paused: AtomicBool::new(false),
            device_lost: AtomicBool::new(false),
            volume: AtomicU32::new(1f32.to_bits()),
            normalise: AtomicU8::new(Normalise::Normal as u8),
            loudness_gain: AtomicU32::new(1f32.to_bits()),
            download: Mutex::new(None),
            next_download: Mutex::new(None),
            device: Mutex::new(None),
            speed: AtomicU32::new(1f32.to_bits()),
        });
        let (tx, rx) = mpsc::channel();
        let engine_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("audio-engine".into())
            .spawn(move || Engine::new(rx, engine_shared, Box::new(on_event)).run())
            .expect("audio thread spawns");
        Self { tx, shared }
    }

    /// Loads a track in place of the current one. A pause is left as it is,
    /// so one made while the stream was being found holds.
    pub fn load(&self, spec: StreamSpec) {
        let (download, opening) = self.start(spec);
        // Cancelled before the engine hears, so a decoder blocked on the old
        // download wakes up, unless the old one is playing again.
        let old = self
            .shared
            .download
            .lock()
            .unwrap()
            .replace(Arc::clone(&download));
        self.shared.cancel_next();
        if let Some(old) = old.filter(|old| !Arc::ptr_eq(old, &download)) {
            old.cancel();
        }
        let _ = self.tx.send(Command::Load(opening, download));
    }

    /// Queues the track to follow the current one without a gap, or fading
    /// into it over `crossfade_secs`, replacing any queued before. Its
    /// download starts now.
    pub fn queue_next(&self, spec: StreamSpec, crossfade_secs: f64) {
        self.shared.cancel_next();
        let (download, opening) = self.start(spec);
        *self.shared.next_download.lock().unwrap() = Some(Arc::clone(&download));
        let _ = self
            .tx
            .send(Command::Queue(opening, download, crossfade_secs));
    }

    /// Starts the spec's download, or takes the current one should it hold
    /// all of the same stream, so a replay neither asks nor downloads again.
    fn start(&self, spec: StreamSpec) -> (Arc<Download>, Opening) {
        let download = match spec.input {
            Input::Stream(source, refresh) => self
                .held(&source.url)
                .unwrap_or_else(|| Download::start(source, refresh)),
            Input::File(path) => Download::open(path),
        };
        (download, spec.opening)
    }

    fn held(&self, url: &str) -> Option<Arc<Download>> {
        let current = self.shared.download.lock().unwrap();
        current.as_ref().filter(|d| d.whole(url)).cloned()
    }

    /// Whether the current download holds all of `url`, to play it again.
    pub fn holds(&self, url: &str) -> bool {
        self.held(url).is_some()
    }

    pub fn clear_next(&self) {
        self.shared.cancel_next();
        let _ = self.tx.send(Command::ClearNext);
    }

    pub fn stop(&self) {
        self.shared.cancel_current();
        self.halt();
    }

    /// Stops the sound but keeps the current download, for a replay to load.
    pub fn halt(&self) {
        self.shared.cancel_next();
        let _ = self.tx.send(Command::Stop);
    }

    pub fn seek(&self, secs: f64) {
        let _ = self.tx.send(Command::Seek(secs));
    }

    pub fn set_paused(&self, paused: bool) {
        // The flag silences the callback at once; the engine then stops the
        // device, which cpal does asynchronously. The command must follow
        // every change of the flag, as the engine sleeps while paused.
        self.shared.paused.store(paused, Ordering::Relaxed);
        let _ = self.tx.send(Command::Pause);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared
            .volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Shapes the sound with each band's gain, or plays it as it is.
    pub fn set_equaliser(&self, gains: Option<eq::Gains>) {
        self.shared.output.lock().unwrap().eq.set(gains);
    }

    /// Plays through the device with this cpal ID, or the default.
    pub fn set_output_device(&self, id: Option<String>) {
        *self.shared.device.lock().unwrap() = id;
        let _ = self.tx.send(Command::Reopen);
    }

    /// Record-style: the device opens at the track's rate times the speed and
    /// Windows resamples, so the engine stays in song time.
    pub fn set_speed(&self, speed: f32) {
        let speed = speed.clamp(MIN_SPEED, MAX_SPEED);
        if self.shared.speed.swap(speed.to_bits(), Ordering::Relaxed) != speed.to_bits() {
            let _ = self.tx.send(Command::Reopen);
        }
    }

    pub fn set_normalise(&self, normalise: Normalise) {
        self.shared
            .normalise
            .store(normalise as u8, Ordering::Relaxed);
    }

    pub fn position(&self) -> f64 {
        self.shared.output.lock().unwrap().position()
    }

    pub fn is_buffering(&self) -> bool {
        self.shared.output.lock().unwrap().underrun
    }

    /// Fraction of the current stream downloaded.
    pub fn buffered(&self) -> f32 {
        self.shared
            .download
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0.0, |d| d.progress())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decode::{Decoded, Decoder};
    #[test]
    fn plays_a_file_from_disk() {
        let path = std::env::temp_dir().join(format!("lily-play-{}.wav", std::process::id()));
        crate::local::tests::write_wav(&path, 1);
        let download = Download::open(path.clone());
        let mut decoder = Decoder::open(download.reader(), "").unwrap();
        let mut samples = Vec::new();
        while let Ok(Decoded::Samples) = decoder.decode_next(&mut samples) {}
        std::fs::remove_file(&path).unwrap();
        assert_eq!((decoder.rate, decoder.channels), (8000, 1));
        assert_eq!(samples.len(), 8000, "a second of it");
    }

    #[test]
    fn cancels_a_download_only_once_neither_slot_holds_it() {
        use std::io::Read;
        let missing = std::env::temp_dir()
            .join("lily-no-such-folder")
            .join("a.flac");
        let download = Download::open(missing);
        let slot = || Mutex::new(Some(Arc::clone(&download)));
        let (current, next) = (slot(), slot());
        let error = || download.reader().read(&mut [0; 4]).unwrap_err().to_string();
        cancel(&next, &current);
        assert!(error().contains("couldn't open"), "{}", error());
        cancel(&current, &next);
        assert_eq!(error(), "download cancelled");
    }

    #[test]
    fn applies_the_next_tracks_loudness() {
        let near = |a: f32, b: f32| (a - b).abs() < 0.001;
        let normal = |db| Normalise::Normal.apply(loudness_gain(db));
        let quiet = |db| Normalise::Quiet.apply(loudness_gain(db));
        assert_eq!(normal(None), 1.0, "unmeasured, taken as at the reference");
        assert!(near(normal(Some(6.0)), 0.501), "loud tracks turned down");
        assert!(near(normal(Some(-3.0)), 1.413), "quiet ones turned up");
        assert_eq!(normal(Some(-20.0)), MAX_BOOST, "but only so far");
        assert_eq!(quiet(None), 0.5);
        assert!(near(quiet(Some(6.0)), 0.251), "6 dB under Normal");
        assert!(near(quiet(Some(-9.0)), 1.409));
        assert_eq!(quiet(Some(-20.0)), MAX_BOOST);
        assert_eq!(Normalise::Off.apply(loudness_gain(Some(6.0))), 1.0);
        for level in [Normalise::Off, Normalise::Quiet, Normalise::Normal] {
            assert_eq!(Normalise::from_index(level.index()), level);
        }
    }
}
