//! The engine thread: decodes the current track into the sample queue, and
//! moves on to the queued one without a gap or with a crossfade.

use std::{
    collections::VecDeque,
    f32::consts::FRAC_PI_2,
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::{
    Command, Opening, PlayerEvent, SILENCE_TRIM_SECS, Shared,
    decode::{Decoded, Decoder},
    loudness_gain,
    output::{Output, fill},
    source::{Download, StreamReader},
};

/// Seconds of decoded audio kept ahead of the device.
const BUFFER_AHEAD_SECS: f64 = 2.0;
/// Seconds of audio the device is always given, taken from the end held
/// back for a crossfade if need be: a download too slow to keep up shortens
/// the fade rather than breaking up the sound.
const LOW_WATER_SECS: f64 = 1.0;
/// About -60 dBFS: quieter than this, a frame counts as silence.
const SILENCE: f32 = 0.001;
const IDLE_POLL: Duration = Duration::from_millis(20);
/// Long enough for the device to play out its buffer as silence.
const PAUSE_AFTER: Duration = Duration::from_millis(250);

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
    opened: Receiver<Result<Opened, String>>,
    ready: Option<Opened>,
    /// Samples of the current track to hold back for the crossfade, with
    /// room for its trailing silence: none for gapless, or once the track
    /// opens in a format the current one cannot be mixed with.
    hold: usize,
    /// Samples the crossfade lasts.
    fade: usize,
}

struct Opened {
    decoder: Decoder,
    /// The first samples after the leading silence cut for a crossfade.
    lead: Vec<f32>,
    skipped_secs: f64,
}

/// The end of the track fading out, mixed under the start of the next.
struct Fade {
    tail: Vec<f32>,
    channels: usize,
    /// Samples of the tail mixed so far.
    mixed: usize,
    /// The fading track's loudness correction over the next one's, as the
    /// device applies the next one's to the mix.
    old_gain: f32,
}

impl Fade {
    fn remaining(&self) -> usize {
        self.tail.len() - self.mixed
    }

    fn done(&self) -> bool {
        self.remaining() == 0
    }

    /// Mixes the tail under `samples` in place. Equal-power curves keep the
    /// loudness steady through the overlap, where linear ones dip midway.
    fn mix(&mut self, samples: &mut [f32]) {
        let n = samples.len().min(self.remaining());
        let len = self.tail.len() as f32;
        let tail = &self.tail[self.mixed..self.mixed + n];
        let frames = samples[..n]
            .chunks_exact_mut(self.channels)
            .zip(tail.chunks_exact(self.channels));
        for (i, (new, old)) in frames.enumerate() {
            let t = (self.mixed + i * self.channels) as f32 / len;
            let (fade_in, fade_out) = (t * FRAC_PI_2).sin_cos();
            let fade_out = fade_out * self.old_gain;
            for (sample, old) in new.iter_mut().zip(old) {
                *sample = *sample * fade_in + old * fade_out;
            }
        }
        self.mixed += n;
    }
}

/// Interleaved samples in `secs` of audio, in whole frames.
fn samples_in(secs: f64, rate: u32, channels: usize) -> usize {
    (secs * f64::from(rate)) as usize * channels
}

fn same_format(decoder: &Decoder, output: &Output) -> bool {
    decoder.rate == output.rate && decoder.channels == output.channels
}

fn is_silent(frame: &[f32]) -> bool {
    frame.iter().all(|s| s.abs() < SILENCE)
}

/// Splits a finished track's held samples at the tail to fade from: up to
/// `fade` samples, ending where its trailing silence, at most `trim`
/// samples of it, begins. What comes before the tail moves on to `out`, and
/// the tail keeps the held buffer, so nothing is copied or allocated. With
/// no fade, gapless, everything moves on and no silence is cut.
fn take_tail(
    held: &mut VecDeque<f32>,
    out: &mut VecDeque<f32>,
    channels: usize,
    fade: usize,
    trim: usize,
) -> Vec<f32> {
    if fade > 0 {
        let silent = held
            .make_contiguous()
            .rchunks_exact(channels)
            .take(trim / channels)
            .take_while(|f| is_silent(f))
            .count();
        held.truncate(held.len() - silent * channels);
    }
    let start = held.len().saturating_sub(fade);
    out.extend(held.drain(..start));
    std::mem::take(held).into()
}

/// Opens the queued track and, for a crossfade, decodes up to its first
/// sound, so the fade brings in music rather than silence.
fn open_next(reader: StreamReader, mime: &str, trim: bool) -> Result<Opened, String> {
    let mut decoder = Decoder::open(reader, mime)?;
    let mut lead = Vec::new();
    let mut skipped = 0;
    if trim {
        let limit = (SILENCE_TRIM_SECS * f64::from(decoder.rate)) as usize;
        while lead.is_empty() && skipped < limit {
            if let Decoded::EndOfStream = decoder.decode_next(&mut lead)? {
                break;
            }
            let silent = lead
                .chunks_exact(decoder.channels)
                .take(limit - skipped)
                .take_while(|f| is_silent(f))
                .count();
            lead.drain(..silent * decoder.channels);
            skipped += silent;
        }
    }
    Ok(Opened {
        skipped_secs: skipped as f64 / f64::from(decoder.rate),
        decoder,
        lead,
    })
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
    /// The current track's samples held back from the device while the
    /// queued one may fade in over them.
    held: VecDeque<f32>,
    fade: Option<Fade>,
    scratch: Vec<f32>,
    /// When to stop the device after a pause.
    pause_at: Option<Instant>,
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
            held: VecDeque::new(),
            fade: None,
            scratch: Vec::new(),
            pause_at: None,
        }
    }

    pub(super) fn run(mut self) {
        // Whether a pass since the last command or decode found nothing to do.
        let mut settled = false;
        loop {
            let command = if self.wants_samples() {
                match self.rx.try_recv() {
                    Ok(command) => Some(command),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            } else if settled && self.idle() {
                match self.rx.recv() {
                    Ok(command) => Some(command),
                    Err(_) => return,
                }
            } else {
                match self.rx.recv_timeout(IDLE_POLL) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            settled = false;
            // Ahead of the command, so a resume starts the new device.
            self.reopen_lost_device();
            match command {
                Some(Command::Load(opening, download)) => self.load(opening, download),
                Some(Command::Queue(opening, download, crossfade_secs)) => {
                    self.queue(opening, download, crossfade_secs)
                }
                Some(Command::ClearNext) => self.drop_next(),
                Some(Command::Seek(secs)) => self.seek(secs),
                Some(Command::Stop) => self.clear(),
                Some(Command::Pause) => self.apply_paused(),
                None => {
                    self.pause_when_due();
                    self.poll_next();
                    self.check_advanced();
                    self.try_switch();
                    self.spill();
                    if self.wants_samples() {
                        self.decode_some();
                    } else {
                        self.check_finished();
                        settled = true;
                    }
                }
            }
        }
    }

    /// Whether only a command can change anything now, so the thread may
    /// sleep until one comes rather than wake to look: paused or stopped,
    /// with the device's stop behind it and no queued track still opening.
    /// True only after a pass that found nothing to do, and only while all
    /// the engine acts on in these states reaches it with a command.
    fn idle(&self) -> bool {
        let opening = self.next.as_ref().is_some_and(|n| n.ready.is_none());
        let stopped = self.track.is_none() && self.previous.is_none();
        self.pause_at.is_none()
            && !opening
            && (stopped || self.shared.paused.load(Ordering::Relaxed))
    }

    fn clear(&mut self) {
        self.track = None;
        self.next = None;
        self.previous = None;
        // Freed rather than emptied, as a crossfade's hold is megabytes.
        self.held = VecDeque::new();
        self.fade = None;
        self.shared.output.lock().unwrap().reset(0.0);
    }

    fn queue(&mut self, opening: Opening, download: Arc<Download>, crossfade_secs: f64) {
        self.drop_next();
        let (rate, channels) = {
            let output = self.shared.output.lock().unwrap();
            (output.rate, output.channels)
        };
        let fade = samples_in(crossfade_secs, rate, channels);
        let hold = if fade > 0 {
            fade + samples_in(SILENCE_TRIM_SECS, rate, channels)
        } else {
            0
        };
        // Sized once, as growing by doubling would overshoot by up to half.
        self.held
            .reserve_exact(hold.saturating_sub(self.held.len()));
        let (tx, opened) = mpsc::channel();
        let reader = download.reader();
        let mime = opening.mime;
        let spawned = thread::Builder::new()
            .name("next-track".into())
            .spawn(move || {
                let _ = tx.send(open_next(reader, &mime, fade > 0));
            });
        if let Err(e) = spawned {
            log::warn!("could not open the next track: {e}");
            return;
        }
        self.next = Some(Next {
            download,
            loudness: loudness_gain(opening.loudness_db),
            opened,
            ready: None,
            hold,
            fade,
        });
    }

    /// Drops the queued track, which may already have taken over decoding.
    fn drop_next(&mut self) {
        self.undo_switch();
        self.next = None;
    }

    fn poll_next(&mut self) {
        let Some(next) = &mut self.next else {
            return;
        };
        if next.ready.is_some() {
            return;
        }
        match next.opened.try_recv() {
            Ok(Ok(opened)) => {
                if !same_format(&opened.decoder, &self.shared.output.lock().unwrap()) {
                    next.hold = 0;
                }
                next.ready = Some(opened);
            }
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
    /// decoded. Of the same format, its samples follow straight on, mixed
    /// over the current track's held tail for a crossfade; otherwise the
    /// device reopens once the current track has played out.
    fn try_switch(&mut self) {
        let finished = self.track.as_ref().is_some_and(|t| t.finished);
        let ready = self.next.as_ref().is_some_and(|n| n.ready.is_some());
        if !finished || !ready || self.previous.is_some() {
            return;
        }
        let mut output = self.shared.output.lock().unwrap();
        let opened = self.next.as_ref().and_then(|n| n.ready.as_ref());
        let same_format = opened.is_some_and(|o| same_format(&o.decoder, &output));
        if !same_format && !(output.samples.is_empty() && self.held.is_empty()) {
            return;
        }
        let Some(Next {
            download,
            loudness,
            fade,
            ready:
                Some(Opened {
                    decoder,
                    mut lead,
                    skipped_secs,
                }),
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
            let trim = samples_in(SILENCE_TRIM_SECS, rate, channels);
            let tail = take_tail(&mut self.held, &mut output.samples, channels, fade, trim);
            let queued = (output.samples.len() / channels) as u64;
            output.switch_at = Some(output.frames_played + queued);
            output.next_loudness = loudness;
            output.next_base_secs = skipped_secs;
            drop(output);
            if !tail.is_empty() {
                let old_gain = if self.shared.normalise.load(Ordering::Relaxed) {
                    f32::from_bits(self.shared.loudness_gain.load(Ordering::Relaxed)) / loudness
                } else {
                    1.0
                };
                self.fade = Some(Fade {
                    tail,
                    channels,
                    mixed: 0,
                    old_gain,
                });
            }
            self.previous = self.track.replace(track);
        } else {
            drop(output);
            if let Err(e) = self.ensure_stream(rate, channels) {
                (self.on_event)(PlayerEvent::Error(e));
                return;
            }
            self.shared.output.lock().unwrap().reset(skipped_secs);
            self.shared.set_loudness(loudness);
            self.track = Some(track);
            self.became_audible();
        }
        self.push(&mut lead);
    }

    /// Undoes a switch the listener has not heard yet: the queued track's
    /// samples leave the queue and the previous track plays out, its tail
    /// held back again as it was before any fade.
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
        self.held = self.fade.take().map(|f| f.tail.into()).unwrap_or_default();
        true
    }

    fn check_advanced(&mut self) {
        let advanced = std::mem::take(&mut self.shared.output.lock().unwrap().advanced);
        if advanced {
            self.previous = None;
            if self.fade.as_ref().is_some_and(Fade::done) {
                self.fade = None;
            }
            self.became_audible();
        }
    }

    fn hold(&self) -> usize {
        self.next.as_ref().map_or(0, |n| n.hold)
    }

    /// Passes decoded samples on to the device, mixed under the tail of the
    /// track fading out, and held back while the queued track may fade in.
    fn push(&mut self, samples: &mut [f32]) {
        if let Some(fade) = &mut self.fade {
            fade.mix(samples);
            // Kept until heard, as undoing the switch restores the tail.
            if fade.done() && self.previous.is_none() {
                self.fade = None;
            }
        }
        if self.held.is_empty() && self.hold() == 0 {
            let mut output = self.shared.output.lock().unwrap();
            output.samples.extend(samples.iter().copied());
        } else {
            self.held.extend(samples.iter().copied());
            self.spill();
        }
    }

    /// Moves held samples on to the device beyond what the crossfade needs,
    /// and whatever the device needs to stay above its low water mark.
    fn spill(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let mut output = self.shared.output.lock().unwrap();
        let low = samples_in(LOW_WATER_SECS, output.rate, output.channels);
        let short = low.saturating_sub(output.samples.len());
        let keep = self.hold().saturating_sub(short);
        if let Some(excess) = self.held.len().checked_sub(keep) {
            output.samples.extend(self.held.drain(..excess));
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
        ahead < BUFFER_AHEAD_SECS || self.held.len() < self.hold()
    }

    fn load(&mut self, opening: Opening, download: Arc<Download>) {
        self.clear();
        self.shared.set_loudness(loudness_gain(opening.loudness_db));

        let mut decoder = match Decoder::open(download.reader(), &opening.mime) {
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
            opening.mime,
            decoder.rate,
            decoder.channels
        );
        if opening.start_at > 0.0 {
            match decoder.seek(opening.start_at) {
                Ok(reached) => self.shared.output.lock().unwrap().reset(reached),
                Err(e) => log::warn!("could not resume at {:.0} s: {e}", opening.start_at),
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
                self.held.clear();
                self.fade = None;
                self.shared.output.lock().unwrap().reset(reached);
            }
            Err(e) => log::warn!("{e}"),
        }
    }

    fn decode_some(&mut self) {
        let Some(track) = &mut self.track else {
            return;
        };
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        match track.decoder.decode_next(&mut scratch) {
            Ok(Decoded::Samples) => {
                let first = !std::mem::replace(&mut track.started, true);
                self.push(&mut scratch);
                if first {
                    (self.on_event)(PlayerEvent::Started);
                }
            }
            Ok(Decoded::EndOfStream) => {
                track.finished = true;
                // A track shorter than the fade ends inside it, and the
                // rest of the tail fades out over silence.
                if let Some(remaining) = self.fade.as_ref().map(Fade::remaining) {
                    scratch.resize(remaining, 0.0);
                    self.push(&mut scratch);
                }
            }
            Err(e) => {
                track.finished = true;
                // Errors from a download cancelled by a newer load are expected.
                let download = Arc::clone(&track.download);
                if self.is_current(&download) {
                    (self.on_event)(PlayerEvent::Error(e));
                }
            }
        }
        self.scratch = scratch;
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
        // A device open at another format closes before the format changes
        // under its callback, and a lost one closes to be opened afresh.
        if self.shared.device_lost.load(Ordering::Relaxed)
            || !matches!(&self.stream, Some((_, r, c)) if *r == rate && *c == channels)
        {
            self.stream = None;
            // Closing waits for the callbacks to end, so nothing of the old
            // stream can report it lost after this.
            self.shared.device_lost.store(false, Ordering::Relaxed);
        }
        {
            let mut output = self.shared.output.lock().unwrap();
            output.rate = rate;
            output.channels = channels;
        }
        if self.stream.is_some() {
            return Ok(());
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
        let lost = Arc::clone(&self.shared);
        let stream = device
            .build_output_stream::<f32, _, _>(
                config,
                move |data, _| fill(&shared, data),
                move |e| {
                    log::warn!("audio output error: {e}");
                    // The kinds that say the device went or another became
                    // the default. The rest cannot be told from a refused
                    // priority boost, which leaves the stream playing; one
                    // they ended is found when it next fails to start.
                    if matches!(
                        e.kind(),
                        cpal::ErrorKind::DeviceNotAvailable
                            | cpal::ErrorKind::StreamInvalidated
                            | cpal::ErrorKind::HostUnavailable
                    ) {
                        lost.device_lost.store(true, Ordering::Relaxed);
                    }
                },
                None,
            )
            .map_err(|e| format!("could not open audio output: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("could not start audio output: {e}"))?;
        self.stream = Some((stream, rate, channels));
        // A format change reached while paused opens a stream that must not run.
        self.apply_paused();
        Ok(())
    }

    /// Opens the device again once its stream is lost, as when the output is
    /// unplugged or another becomes the default, which cpal's stream does
    /// not follow. The queued samples carry on from where the old one stopped.
    fn reopen_lost_device(&mut self) {
        if !self.shared.device_lost.load(Ordering::Relaxed) {
            return;
        }
        let (rate, channels) = {
            let output = self.shared.output.lock().unwrap();
            (output.rate, output.channels)
        };
        if let Err(e) = self.ensure_stream(rate, channels) {
            // shortcut: asks again every pass, 50 times a second, while
            // there is no device to open; time the retries if that shows.
            log::debug!("{e}");
            self.shared.device_lost.store(true, Ordering::Relaxed);
        }
    }

    /// Stops the device while paused rather than feeding it silence: a running
    /// stream wakes its callback every few ms and holds a power request that
    /// keeps Windows from sleeping. The stop waits `PAUSE_AFTER`, as WASAPI
    /// keeps its buffer through a stop and would replay the audio left in it
    /// on the next start, a blip of the old track after a skip or seek.
    fn apply_paused(&mut self) {
        let Some((stream, ..)) = &self.stream else {
            return;
        };
        if self.shared.paused.load(Ordering::Relaxed) {
            self.pause_at = Some(Instant::now() + PAUSE_AFTER);
            return;
        }
        self.pause_at = None;
        if let Err(e) = stream.play() {
            log::warn!("could not resume audio output: {e}");
            // Its thread has ended, whatever the error that ended it.
            self.shared.device_lost.store(true, Ordering::Relaxed);
        }
    }

    /// Stops the device once it has played out silence since the pause, and
    /// only with nothing left to decode: a decode can block on a slow
    /// download, and the resume that restarts the device would wait behind it.
    fn pause_when_due(&mut self) {
        if self.pause_at.is_none_or(|at| Instant::now() < at) || self.wants_samples() {
            return;
        }
        self.pause_at = None;
        if let Some((stream, ..)) = &self.stream
            && self.shared.paused.load(Ordering::Relaxed)
            && let Err(e) = stream.pause()
        {
            log::warn!("could not pause audio output: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fade(tail: Vec<f32>, old_gain: f32) -> Fade {
        Fade {
            tail,
            channels: 2,
            mixed: 0,
            old_gain,
        }
    }

    #[test]
    fn fades_with_equal_power() {
        let mut fade = fade(vec![1.0; 2_000], 1.0);
        let mut samples = vec![1.0; 2_000];
        fade.mix(&mut samples);
        assert!(fade.done());
        assert_eq!(samples[..2], [1.0, 1.0], "starts on the old track alone");
        for [left, right] in samples.as_chunks::<2>().0 {
            assert_eq!(left, right, "channels fade together");
        }
        // Uncorrelated tracks sum by power, so the curves' squares sum to 1.
        let i = 500 * 2;
        let t = 500.0 / 1_000.0 * FRAC_PI_2;
        let (fade_in, fade_out) = t.sin_cos();
        assert!((fade_in * fade_in + fade_out * fade_out - 1.0).abs() < 1e-6);
        assert!((samples[i] - (fade_in + fade_out)).abs() < 1e-6);
        assert!(
            samples[1_998] > 0.99 && samples[1_998] < 1.01,
            "ends near the new track alone"
        );
    }

    #[test]
    fn mixes_across_packets_as_in_one() {
        let tail: Vec<f32> = (0..960).map(|i| (i as f32 * 0.01).sin()).collect();
        let new: Vec<f32> = (0..960).map(|i| (i as f32 * 0.03).cos()).collect();
        let mut whole = new.clone();
        fade(tail.clone(), 0.5).mix(&mut whole);

        let mut split = fade(tail, 0.5);
        let mut parts = new;
        for part in parts.chunks_mut(122) {
            split.mix(part);
        }
        assert_eq!(parts, whole);
    }

    #[test]
    fn leaves_samples_past_the_fade_alone() {
        let mut fade = fade(vec![0.5; 4], 2.0);
        let mut samples = vec![0.25; 10];
        fade.mix(&mut samples);
        assert!(fade.done());
        assert_eq!(samples[0], 1.0, "the old track at its relative loudness");
        assert_eq!(samples[4..], [0.25; 6]);
    }

    #[test]
    fn splits_the_tail_before_trailing_silence() {
        let mut held: VecDeque<f32> = [0.25; 4]
            .into_iter()
            .chain([0.5; 4])
            .chain([0.0; 6])
            .collect();
        let mut out = VecDeque::new();
        let tail = take_tail(&mut held, &mut out, 2, 4, 20);
        assert_eq!(tail, [0.5; 4]);
        assert_eq!(out, [0.25; 4], "the rest plays before the fade");
        assert!(held.is_empty());
    }

    #[test]
    fn cuts_no_more_silence_than_allowed() {
        let mut held: VecDeque<f32> = [0.5; 4].into_iter().chain([0.0; 8]).collect();
        let mut out = VecDeque::new();
        let tail = take_tail(&mut held, &mut out, 2, 4, 2);
        assert_eq!(tail, [0.0; 4], "a long silence is the music's, not the end");
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn gapless_takes_no_tail() {
        let mut held: VecDeque<f32> = [0.5, 0.5, 0.0, 0.0].into_iter().collect();
        let mut out = VecDeque::new();
        assert!(take_tail(&mut held, &mut out, 2, 0, 20).is_empty());
        assert_eq!(
            out,
            [0.5, 0.5, 0.0, 0.0],
            "gapless keeps the silence, exactly"
        );
    }

    #[test]
    fn a_fade_longer_than_the_track_takes_all_of_it() {
        let mut held: VecDeque<f32> = [0.5; 6].into_iter().collect();
        let mut out = VecDeque::new();
        assert_eq!(take_tail(&mut held, &mut out, 2, 200, 20).len(), 6);
        assert!(out.is_empty());
    }

    #[test]
    fn hears_silence_below_the_threshold_only() {
        assert!(is_silent(&[0.0, -0.000_5]));
        assert!(!is_silent(&[0.0, 0.01]));
    }
}
