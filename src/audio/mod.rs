//! Audio engine: one thread decodes the current stream into a sample queue
//! that the output device drains.

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
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// The track has started producing sound.
    Started,
    /// The track played to the end.
    Ended,
    Error(String),
}

enum Command {
    Load(StreamSpec, Arc<Download>),
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
    }
}

struct Shared {
    output: Mutex<Output>,
    paused: AtomicBool,
    /// Linear gain as f32 bits: volume curve times loudness correction.
    gain: AtomicU32,
    volume: AtomicU32,
    normalise: AtomicBool,
    loudness_gain: AtomicU32,
    download: Mutex<Option<Arc<Download>>>,
}

impl Shared {
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
            }),
            paused: AtomicBool::new(false),
            gain: AtomicU32::new(1f32.to_bits()),
            volume: AtomicU32::new(1f32.to_bits()),
            normalise: AtomicBool::new(true),
            loudness_gain: AtomicU32::new(1f32.to_bits()),
            download: Mutex::new(None),
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
        if let Some(old) = self.shared.download.lock().unwrap().take() {
            old.cancel();
        }
        let download = Download::start(spec.url.clone(), spec.size, spec.headers.clone());
        *self.shared.download.lock().unwrap() = Some(Arc::clone(&download));
        self.shared.paused.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::Load(spec, download));
    }

    pub fn stop(&self) {
        if let Some(old) = self.shared.download.lock().unwrap().take() {
            old.cancel();
        }
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

struct Engine {
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    on_event: Box<dyn Fn(PlayerEvent) + Send>,
    stream: Option<(cpal::Stream, u32, usize)>,
    track: Option<Track>,
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
                Some(Command::Seek(secs)) => self.seek(secs),
                Some(Command::Stop) => {
                    self.track = None;
                    self.shared.output.lock().unwrap().reset(0.0);
                }
                None if self.wants_samples() => self.decode_some(),
                None => self.check_finished(),
            }
        }
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
        self.shared.output.lock().unwrap().reset(0.0);

        let loudness_gain = spec
            .loudness_db
            .filter(|db| *db > 0.0)
            .map_or(1.0, |db| 10f32.powf(-db / 20.0));
        self.shared
            .loudness_gain
            .store(loudness_gain.to_bits(), Ordering::Relaxed);
        self.shared.update_gain();

        let decoder = match Decoder::open(download.reader(), &spec.mime) {
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
        if track.finished && self.shared.output.lock().unwrap().samples.is_empty() {
            self.track = None;
            (self.on_event)(PlayerEvent::Ended);
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
    output.frames_played += (n / channels) as u64;
    output.underrun = n < data.len() && output.rate > 0;
}
