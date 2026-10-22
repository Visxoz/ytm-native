//! The engine thread: decodes the current track into the sample queue, and
//! moves on to the queued one without a gap.

use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::{
    Command, PlayerEvent, Shared, StreamSpec,
    decode::{Decoded, Decoder},
    loudness_gain,
    output::fill,
    source::Download,
};

/// Seconds of decoded audio kept ahead of the device.
const BUFFER_AHEAD_SECS: f64 = 2.0;
const IDLE_POLL: Duration = Duration::from_millis(20);

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

pub(super) struct Engine {
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
    pub(super) fn new(
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

    pub(super) fn run(mut self) {
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
