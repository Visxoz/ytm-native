//! Audio engine: one thread decodes the current stream into a sample queue
//! that the output device drains.
//!
//! Gapless playback: the app queues the next track shortly before the
//! current one ends. Its download starts at once and its decoder opens on a
//! side thread, so the engine never waits on the network. When the current
//! track runs out, the next one's samples follow in the same queue, and the
//! device callback marks the sample where it becomes audible; only then does
//! the app hear that the track changed.

pub mod decode;
mod opus;
mod source;

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use decode::{Decoded, Decoder};
use source::Download;

/// Seconds of decoded audio kept ahead of the device.
const BUFFER_AHEAD_SECS: f64 = 2.0;
const IDLE_POLL: Duration = Duration::from_millis(20);

/// A stream to play, as returned by the player endpoint.
pub struct StreamSpec {
    pub url: String,
    pub size: u64,
    pub mime: String,
    pub headers: Vec<(String, String)>,
    /// Integrated loudness YouTube measured, used to even out volume.
    pub loudness_db: Option<f32>,
    /// Where to start, in seconds, when resuming a track.
    pub start_at: f64,
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
    Load(StreamSpec, Arc<Download>),
    Queue(StreamSpec, Arc<Download>),
    ClearNext,
    Seek(f64),
    Stop,
}

/// State shared with the device callback.
struct Output {
    samples: VecDeque<f32>,
    rate: u32,
    channels: usize,
    /// Position of the first queued sample, in seconds.
    base_secs: f64,
    frames_played: u64,
    /// Set by the device callback when the queue ran dry.
    underrun: bool,
    /// The frame count at which the queued track's samples start, once its
    /// samples follow the current track's in the queue.
    switch_at: Option<u64>,
    /// The queued track's loudness correction, applied as it starts.
    next_loudness: f32,
    /// Set by the device callback once the queued track is audible.
    advanced: bool,
}

impl Output {
    fn position(&self) -> f64 {
        if self.rate == 0 {
            return self.base_secs;
        }
        self.base_secs + self.frames_played as f64 / self.rate as f64
    }

    fn reset(&mut self, base_secs: f64) {
        self.samples.clear();
        self.base_secs = base_secs;
        self.frames_played = 0;
        self.switch_at = None;
    }

    /// Counts `frames` as played. Returns the next track's loudness
    /// correction when they cross into it, which then counts from zero.
    fn played(&mut self, frames: u64) -> Option<f32> {
        self.frames_played += frames;
        let at = self.switch_at?;
        if self.frames_played < at {
            return None;
        }
        self.frames_played -= at;
        self.base_secs = 0.0;
        self.switch_at = None;
        self.advanced = true;
        Some(self.next_loudness)
    }
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
        let download = Download::start(spec.url.clone(), spec.size, spec.headers.clone());
        *self.shared.download.lock().unwrap() = Some(Arc::clone(&download));
        self.shared.paused.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::Load(spec, download));
    }

    /// Queues the track to follow the current one without a gap, replacing
    /// any queued before. Its download starts now.
    pub fn queue_next(&self, spec: StreamSpec) {
        self.shared.cancel_next();
        let download = Download::start(spec.url.clone(), spec.size, spec.headers.clone());
        *self.shared.next_download.lock().unwrap() = Some(Arc::clone(&download));
        let _ = self.tx.send(Command::Queue(spec, download));
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

struct Track {
    decoder: Decoder,
    download: Arc<Download>,
    finished: bool,
    started: bool,
}

/// The queued track, whose decoder opens on a side thread, as opening reads
/// the first of the download and would block the engine.
struct Next {
    download: Arc<Download>,
    loudness: f32,
    opened: Receiver<Result<Decoder, String>>,
    decoder: Option<Decoder>,
}

struct Engine {
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    on_event: Box<dyn Fn(PlayerEvent) + Send>,
    stream: Option<(cpal::Stream, u32, usize)>,
    track: Option<Track>,
    next: Option<Next>,
    /// The track still audible after the queued one took over decoding,
    /// kept so a seek before the switch can return to it.
    previous: Option<Track>,
    scratch: Vec<f32>,
}

impl Engine {
    fn new(
        rx: Receiver<Command>,
        shared: Arc<Shared>,
        on_event: Box<dyn Fn(PlayerEvent) + Send>,
    ) -> Self {
        Self {
            rx,
            shared,
            on_event,
            stream: None,
            track: None,
            next: None,
            previous: None,
            scratch: Vec::new(),
        }
    }

    fn run(mut self) {
        loop {
            let command = if self.wants_samples() {
                match self.rx.try_recv() {
                    Ok(command) => Some(command),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            } else {
                match self.rx.recv_timeout(IDLE_POLL) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            match command {
                Some(Command::Load(spec, download)) => self.load(spec, download),
                Some(Command::Queue(spec, download)) => self.queue(spec, download),
                Some(Command::ClearNext) => {
                    self.undo_switch();
                    self.next = None;
                }
                Some(Command::Seek(secs)) => self.seek(secs),
                Some(Command::Stop) => {
                    self.track = None;
                    self.next = None;
                    self.previous = None;
                    self.shared.output.lock().unwrap().reset(0.0);
                }
                None => {
                    self.poll_next();
                    self.check_advanced();
                    self.try_switch();
                    if self.wants_samples() {
                        self.decode_some();
                    } else {
                        self.check_finished();
                    }
                }
            }
        }
    }

    fn queue(&mut self, spec: StreamSpec, download: Arc<Download>) {
        // A replaced queued track may already have taken over decoding.
        self.undo_switch();
        self.next = None;
        let (tx, opened) = mpsc::channel();
        let reader = download.reader();
        let mime = spec.mime;
        let spawned = thread::Builder::new()
            .name("next-track".into())
            .spawn(move || {
                let _ = tx.send(Decoder::open(reader, &mime));
            });
        if let Err(e) = spawned {
            log::warn!("could not open the next track: {e}");
            return;
        }
        self.next = Some(Next {
            download,
            loudness: loudness_gain(spec.loudness_db),
            opened,
            decoder: None,
        });
    }

    fn poll_next(&mut self) {
        let Some(next) = &mut self.next else {
            return;
        };
        if next.decoder.is_some() {
            return;
        }
        match next.opened.try_recv() {
            Ok(Ok(decoder)) => next.decoder = Some(decoder),
            // The track then loads as usual once the current one ends.
            Ok(Err(e)) => {
                log::debug!("the next track did not open: {e}");
                self.next = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.next = None,
        }
    }

    /// Moves decoding on to the queued track once the current one is fully
    /// decoded. Of the same format, its samples follow straight on;
    /// otherwise the device reopens once the current track has played out.
    fn try_switch(&mut self) {
        let finished = self.track.as_ref().is_some_and(|t| t.finished);
        let ready = self.next.as_ref().is_some_and(|n| n.decoder.is_some());
        if !finished || !ready || self.previous.is_some() {
            return;
        }
        let mut output = self.shared.output.lock().unwrap();
        let decoder = self.next.as_ref().and_then(|n| n.decoder.as_ref());
        let same_format =
            decoder.is_some_and(|d| d.rate == output.rate && d.channels == output.channels);
        if !same_format && !output.samples.is_empty() {
            return;
        }
        let Some(Next {
            download,
            loudness,
            decoder: Some(decoder),
            ..
        }) = self.next.take()
        else {
            return;
        };
        let (rate, channels) = (decoder.rate, decoder.channels);
        let track = Track {
            decoder,
            download,
            finished: false,
            started: true,
        };
        if same_format {
            let queued = (output.samples.len() / output.channels.max(1)) as u64;
            output.switch_at = Some(output.frames_played + queued);
            output.next_loudness = loudness;
            drop(output);
            self.previous = self.track.replace(track);
        } else {
            drop(output);
            if let Err(e) = self.ensure_stream(rate, channels) {
                (self.on_event)(PlayerEvent::Error(e));
                return;
            }
            self.shared.output.lock().unwrap().reset(0.0);
            self.shared.set_loudness(loudness);
            self.track = Some(track);
            self.became_audible();
        }
    }

    /// Undoes a switch the listener has not heard yet: the queued track's
    /// samples leave the queue and the previous track plays out.
    fn undo_switch(&mut self) -> bool {
        let Some(previous) = self.previous.take() else {
            return false;
        };
        if let Some(switched) = self.track.replace(previous) {
            switched.download.cancel();
        }
        let mut output = self.shared.output.lock().unwrap();
        if let Some(at) = output.switch_at.take() {
            let frames = at.saturating_sub(output.frames_played) as usize;
            let keep = frames * output.channels;
            output.samples.truncate(keep);
        }
        true
    }

    fn check_advanced(&mut self) {
        let advanced = std::mem::take(&mut self.shared.output.lock().unwrap().advanced);
        if advanced {
            self.previous = None;
            self.became_audible();
        }
    }

    /// The current track's download becomes the audible one, for progress.
    fn became_audible(&self) {
        let Some(download) = self.track.as_ref().map(|t| Arc::clone(&t.download)) else {
            return;
        };
        {
            let mut next = self.shared.next_download.lock().unwrap();
            if next.as_ref().is_some_and(|n| Arc::ptr_eq(n, &download)) {
                *next = None;
            }
        }
        *self.shared.download.lock().unwrap() = Some(download);
        (self.on_event)(PlayerEvent::Advanced);
    }

    fn wants_samples(&self) -> bool {
        let Some(track) = &self.track else {
            return false;
        };
        if track.finished {
            return false;
        }
        let output = self.shared.output.lock().unwrap();
        let ahead =
            output.samples.len() as f64 / (output.rate.max(1) as usize * output.channels) as f64;
        ahead < BUFFER_AHEAD_SECS
    }

    fn load(&mut self, spec: StreamSpec, download: Arc<Download>) {
        self.track = None;
        self.next = None;
        self.previous = None;
        self.shared.output.lock().unwrap().reset(0.0);
        self.shared.set_loudness(loudness_gain(spec.loudness_db));

        let mut decoder = match Decoder::open(download.reader(), &spec.mime) {
            Ok(decoder) => decoder,
            Err(e) => {
                // A newer load cancelled this one; stay quiet.
                if !self.is_current(&download) {
                    return;
                }
                (self.on_event)(PlayerEvent::Error(e));
                return;
            }
        };
        if let Err(e) = self.ensure_stream(decoder.rate, decoder.channels) {
            (self.on_event)(PlayerEvent::Error(e));
            return;
        }
        log::debug!(
            "playing {} at {} Hz, {} ch",
            spec.mime,
            decoder.rate,
            decoder.channels
        );
        if spec.start_at > 0.0 {
            match decoder.seek(spec.start_at) {
                Ok(reached) => self.shared.output.lock().unwrap().reset(reached),
                Err(e) => log::warn!("could not resume at {:.0} s: {e}", spec.start_at),
            }
        }
        self.track = Some(Track {
            decoder,
            download,
            finished: false,
            started: false,
        });
    }

    fn is_current(&self, download: &Arc<Download>) -> bool {
        self.shared
            .download
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|d| Arc::ptr_eq(d, download))
    }

    fn seek(&mut self, secs: f64) {
        // A seek lands in the track still audible, so a queued track that
        // already took over decoding is dropped, and the app queues it again.
        if self.undo_switch() {
            (self.on_event)(PlayerEvent::NextDropped);
        }
        let Some(track) = &mut self.track else {
            return;
        };
        match track.decoder.seek(secs) {
            Ok(reached) => {
                track.finished = false;
                self.shared.output.lock().unwrap().reset(reached);
            }
            Err(e) => log::warn!("{e}"),
        }
    }

    fn decode_some(&mut self) {
        let Some(track) = &mut self.track else {
            return;
        };
        self.scratch.clear();
        match track.decoder.decode_next(&mut self.scratch) {
            Ok(Decoded::Samples) => {
                self.shared
                    .output
                    .lock()
                    .unwrap()
                    .samples
                    .extend(self.scratch.iter().copied());
                if !track.started {
                    track.started = true;
                    (self.on_event)(PlayerEvent::Started);
                }
            }
            Ok(Decoded::EndOfStream) => track.finished = true,
            Err(e) => {
                track.finished = true;
                // Errors from a download cancelled by a newer load are expected.
                let download = Arc::clone(&track.download);
                if self.is_current(&download) {
                    (self.on_event)(PlayerEvent::Error(e));
                }
            }
        }
    }

    fn check_finished(&mut self) {
        let Some(track) = &self.track else {
            return;
        };
        // A queued track still opening takes over once it can.
        if self.next.is_some() {
            return;
        }
        if track.finished && self.shared.output.lock().unwrap().samples.is_empty() {
            // A stream that ends without a sound is broken, not finished: as
            // "ended" it would skip through the queue fetching every track.
            let event = if track.started {
                PlayerEvent::Ended
            } else {
                PlayerEvent::Error("the stream ended before playing".into())
            };
            self.track = None;
            (self.on_event)(event);
        }
    }

    /// Opens the device at the track's format; Windows resamples if needed.
    fn ensure_stream(&mut self, rate: u32, channels: usize) -> Result<(), String> {
        if let Some((_, r, c)) = &self.stream
            && *r == rate
            && *c == channels
        {
            let mut output = self.shared.output.lock().unwrap();
            output.rate = rate;
            output.channels = channels;
            return Ok(());
        }
        self.stream = None;
        {
            let mut output = self.shared.output.lock().unwrap();
            output.rate = rate;
            output.channels = channels;
        }

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no audio output device")?;
        let config = cpal::StreamConfig {
            channels: channels as u16,
            sample_rate: rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let shared = Arc::clone(&self.shared);
        let stream = device
            .build_output_stream::<f32, _, _>(
                config,
                move |data, _| fill(&shared, data),
                |e| log::warn!("audio output error: {e}"),
                None,
            )
            .map_err(|e| format!("could not open audio output: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("could not start audio output: {e}"))?;
        self.stream = Some((stream, rate, channels));
        Ok(())
    }
}

/// Device callback: drains the queue, or writes silence while paused.
fn fill(shared: &Shared, data: &mut [f32]) {
    if shared.paused.load(Ordering::Relaxed) {
        data.fill(0.0);
        return;
    }
    let gain = f32::from_bits(shared.gain.load(Ordering::Relaxed));
    let mut output = shared.output.lock().unwrap();
    let channels = output.channels.max(1);
    let n = data.len().min(output.samples.len());
    let n = n - n % channels;
    for (out, sample) in data.iter_mut().zip(output.samples.drain(..n)) {
        *out = sample * gain;
    }
    data[n..].fill(0.0);
    if let Some(loudness) = output.played((n / channels) as u64) {
        shared.set_loudness(loudness);
    }
    output.underrun = n < data.len() && output.rate > 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> Output {
        Output {
            samples: VecDeque::new(),
            rate: 48_000,
            channels: 2,
            base_secs: 30.0,
            frames_played: 0,
            underrun: false,
            switch_at: None,
            next_loudness: 0.5,
            advanced: false,
        }
    }

    #[test]
    fn counts_frames_without_a_queued_track() {
        let mut out = output();
        assert_eq!(out.played(48_000), None);
        assert_eq!(out.position(), 31.0);
        assert!(!out.advanced);
    }

    #[test]
    fn crosses_into_the_queued_track_at_its_first_frame() {
        let mut out = output();
        out.switch_at = Some(1_000);
        assert_eq!(out.played(600), None, "still the current track");
        assert_eq!(out.played(600), Some(0.5), "crossed, with its loudness");
        assert!(out.advanced);
        assert_eq!(out.switch_at, None);
        assert_eq!(out.frames_played, 200, "the next track's frames so far");
        assert_eq!(out.base_secs, 0.0);
    }

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
