//! Audio engine: one thread decodes the current stream into a sample queue
//! that the output device drains.
//!
//! Gapless playback: the app queues the next track shortly before the
//! current one ends. Its download starts at once and its decoder opens on a
//! side thread, so the engine never waits on the network. When the current
//! track runs out, the next one's samples follow in the same queue, and the
//! device callback marks the sample where it becomes audible; only then does
//! the app hear that the track changed.
//!
//! Crossfade: with a queued track to fade into, the engine holds the end of
//! the current one back from the device. Once it is decoded to the end, its
//! trailing silence is cut, and its last seconds are mixed under the next
//! track's first, whose leading silence was cut as it opened. The mix sits
//! in the queue like any samples, so the device callback does no more work.

pub mod decode;
mod engine;
mod opus;
mod output;
mod source;

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Sender},
    },
    thread,
};

use engine::Engine;
use output::Output;
use source::Download;
pub use source::Source;

/// The most silence cut from either side of a crossfade, so a quiet
/// passage or a hidden track is not mistaken for the end.
pub const SILENCE_TRIM_SECS: f64 = 4.0;

/// A stream to play: where its bytes come from and what the engine opens
/// them with.
pub struct StreamSpec {
    pub source: Source,
    pub opening: Opening,
}

/// What the engine opens a stream with, once the download has its source.
pub struct Opening {
    pub mime: String,
    /// Integrated loudness YouTube measured, used to even out volume.
    pub loudness_db: Option<f32>,
    /// Where to start, in seconds, when resuming a track.
    pub start_at: f64,
}

impl StreamSpec {
    /// Starts the download, handing it the source rather than a copy.
    fn start(self) -> (Arc<Download>, Opening) {
        (Download::start(self.source), self.opening)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// The track has started producing sound.
    Started,
    /// The queued track has become audible, with no gap.
    Advanced,
    /// The queued track was dropped, as a seek kept the current one going.
    NextDropped,
    /// The track played to the end, with nothing queued.
    Ended,
    Error(String),
}

enum Command {
    Load(Opening, Arc<Download>),
    Queue(Opening, Arc<Download>, f64),
    ClearNext,
    Seek(f64),
    Stop,
}

/// Empties a download slot and cancels what it held, once the slot's lock
/// is released.
fn cancel(slot: &Mutex<Option<Arc<Download>>>) {
    let taken = slot.lock().unwrap().take();
    if let Some(download) = taken {
        download.cancel();
    }
}

fn loudness_gain(loudness_db: Option<f32>) -> f32 {
    loudness_db
        .filter(|db| *db > 0.0)
        .map_or(1.0, |db| 10f32.powf(-db / 20.0))
}

struct Shared {
    output: Mutex<Output>,
    paused: AtomicBool,
    /// Linear gain as f32 bits: volume curve times loudness correction.
    gain: AtomicU32,
    volume: AtomicU32,
    normalise: AtomicBool,
    loudness_gain: AtomicU32,
    /// The audible track's download.
    download: Mutex<Option<Arc<Download>>>,
    /// The queued track's download.
    next_download: Mutex<Option<Arc<Download>>>,
}

impl Shared {
    fn set_loudness(&self, gain: f32) {
        self.loudness_gain.store(gain.to_bits(), Ordering::Relaxed);
        self.update_gain();
    }

    fn cancel_current(&self) {
        cancel(&self.download);
    }

    fn cancel_next(&self) {
        cancel(&self.next_download);
    }

    fn update_gain(&self) {
        let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
        let loudness = if self.normalise.load(Ordering::Relaxed) {
            f32::from_bits(self.loudness_gain.load(Ordering::Relaxed))
        } else {
            1.0
        };
        // Squared so the slider feels even to the ear.
        self.gain
            .store((volume * volume * loudness).to_bits(), Ordering::Relaxed);
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
                samples: VecDeque::new(),
                rate: 0,
                channels: 2,
                base_secs: 0.0,
                frames_played: 0,
                underrun: false,
                switch_at: None,
                next_loudness: 1.0,
                next_base_secs: 0.0,
                advanced: false,
            }),
            paused: AtomicBool::new(false),
            gain: AtomicU32::new(1f32.to_bits()),
            volume: AtomicU32::new(1f32.to_bits()),
            normalise: AtomicBool::new(true),
            loudness_gain: AtomicU32::new(1f32.to_bits()),
            download: Mutex::new(None),
            next_download: Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel();
        let engine_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("audio-engine".into())
            .spawn(move || Engine::new(rx, engine_shared, Box::new(on_event)).run())
            .expect("audio thread spawns");
        Self { tx, shared }
    }

    pub fn load(&self, spec: StreamSpec) {
        // Cancel first so a decoder blocked on the old download wakes up.
        self.shared.cancel_current();
        self.shared.cancel_next();
        let (download, opening) = spec.start();
        *self.shared.download.lock().unwrap() = Some(Arc::clone(&download));
        self.shared.paused.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::Load(opening, download));
    }

    /// Queues the track to follow the current one without a gap, or fading
    /// into it over `crossfade_secs`, replacing any queued before. Its
    /// download starts now.
    pub fn queue_next(&self, spec: StreamSpec, crossfade_secs: f64) {
        self.shared.cancel_next();
        let (download, opening) = spec.start();
        *self.shared.next_download.lock().unwrap() = Some(Arc::clone(&download));
        let _ = self
            .tx
            .send(Command::Queue(opening, download, crossfade_secs));
    }

    pub fn clear_next(&self) {
        self.shared.cancel_next();
        let _ = self.tx.send(Command::ClearNext);
    }

    pub fn stop(&self) {
        self.shared.cancel_current();
        self.shared.cancel_next();
        let _ = self.tx.send(Command::Stop);
    }

    pub fn seek(&self, secs: f64) {
        let _ = self.tx.send(Command::Seek(secs));
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Sets the volume from 0 to 1.
    pub fn set_volume(&self, volume: f32) {
        self.shared
            .volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.shared.update_gain();
    }

    pub fn set_normalise(&self, normalise: bool) {
        self.shared.normalise.store(normalise, Ordering::Relaxed);
        self.shared.update_gain();
    }

    /// Current position in seconds.
    pub fn position(&self) -> f64 {
        self.shared.output.lock().unwrap().position()
    }

    /// Whether playback is waiting on the network.
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

    #[test]
    fn applies_the_next_tracks_loudness() {
        assert_eq!(loudness_gain(None), 1.0);
        assert_eq!(
            loudness_gain(Some(-3.0)),
            1.0,
            "quiet tracks stay as they are"
        );
        assert!((loudness_gain(Some(6.0)) - 0.501).abs() < 0.001);
    }
}
