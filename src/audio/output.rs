//! The sample queue the output device drains, and the device callback.

use std::{collections::VecDeque, sync::atomic::Ordering};

use super::{Shared, eq::Equaliser, stereo::Stereo};

/// State shared with the device callback.
#[derive(Default)]
pub(super) struct Output {
    pub(super) samples: VecDeque<f32>,
    pub(super) rate: u32,
    pub(super) channels: usize,
    /// Position of the first queued sample, in seconds.
    pub(super) base_secs: f64,
    pub(super) frames_played: u64,
    /// Set by the device callback when the queue ran dry.
    pub(super) underrun: bool,
    /// The frame count at which the queued track's samples start, once its
    /// samples follow the current track's in the queue.
    pub(super) switch_at: Option<u64>,
    /// The queued track's loudness correction, applied as it starts.
    pub(super) next_loudness: f32,
    /// Where the queued track's first queued sample lies, past any leading
    /// silence cut for a crossfade.
    pub(super) next_base_secs: f64,
    /// Set by the device callback once the queued track is audible.
    pub(super) advanced: bool,
    pub(super) eq: Equaliser,
    pub(super) stereo: Stereo,
    /// The gain the last callback ended on, which the next ramps from, so a
    /// pause, resume, volume change or fresh start never steps and clicks.
    pub(super) gain: f32,
    /// How far the limiter has the gain down from 1, at 0 while idle.
    pub(super) limiting: f32,
}

/// How long the limiter takes to let go of a full reduction, in seconds;
/// smaller ones go in proportion, so a 3 dB dip comes back in about 0.3 s.
const RELEASE_SECS: f32 = 1.0;

/// Keeps peaks within full scale: each frame's gain drops at once to what
/// keeps its loudest channel at 1, then rises back linearly, so it lands
/// exactly on idle and leaves samples already within full scale untouched.
fn limit(samples: &mut [f32], limiting: &mut f32, rate: u32, channels: usize) {
    let step = 1.0 / (RELEASE_SECS * rate.max(1) as f32);
    for frame in samples.chunks_exact_mut(channels) {
        let peak = frame.iter().fold(0f32, |m, s| m.max(s.abs()));
        *limiting = (*limiting - step).max(0.0);
        if peak * (1.0 - *limiting) > 1.0 {
            *limiting = 1.0 - 1.0 / peak;
        }
        if *limiting > 0.0 {
            for sample in frame {
                // Clamped too, as `1 - (1 - 1 / peak)` can round past it.
                *sample = (*sample * (1.0 - *limiting)).clamp(-1.0, 1.0);
            }
        }
    }
}

impl Output {
    pub(super) fn position(&self) -> f64 {
        if self.rate == 0 {
            return self.base_secs;
        }
        self.base_secs + self.frames_played as f64 / self.rate as f64
    }

    pub(super) fn reset(&mut self, base_secs: f64) {
        self.samples.clear();
        self.base_secs = base_secs;
        self.frames_played = 0;
        self.switch_at = None;
        // What follows a seek or load fades in, from no limiting.
        self.gain = 0.0;
        self.limiting = 0.0;
        self.stereo.reset();
    }

    /// Counts `frames` as played. Returns the next track's loudness
    /// correction when they cross into it, which then counts from its start.
    fn played(&mut self, frames: u64) -> Option<f32> {
        self.frames_played += frames;
        let at = self.switch_at?;
        if self.frames_played < at {
            return None;
        }
        self.frames_played -= at;
        self.base_secs = self.next_base_secs;
        self.switch_at = None;
        self.advanced = true;
        Some(self.next_loudness)
    }
}

/// Device callback: drains the queue, fading out into silence while paused.
/// The equaliser runs at `device_rate`, the track's rate times the speed.
pub(super) fn fill(shared: &Shared, data: &mut [f32], device_rate: u32) {
    let paused = shared.paused.load(Ordering::Relaxed);
    let mut output = shared.output.lock().unwrap();
    if paused && output.gain == 0.0 {
        data.fill(0.0);
        return;
    }
    let target = if paused { 0.0 } else { shared.gain() };
    let channels = output.channels.max(1);
    let n = data.len().min(output.samples.len());
    let n = n - n % channels;
    let frames = n / channels;
    // Frames from the queued track's first take its loudness at once, as a
    // crossfade's tail was scaled for it.
    let split = output.switch_at.map_or(frames, |at| {
        at.saturating_sub(output.frames_played).min(frames as u64) as usize
    });
    let next = shared.correction(output.next_loudness) / shared.loudness();
    let from = output.gain;
    let step = (target - from) / frames.max(1) as f32;
    for (i, (out, sample)) in data.iter_mut().zip(output.samples.drain(..n)).enumerate() {
        let frame = i / channels;
        let gain = from + step * (frame + 1) as f32;
        *out = sample * if frame < split { gain } else { gain * next };
    }
    output.eq.process(&mut data[..n], device_rate, channels);
    output.stereo.process(&mut data[..n], device_rate, channels);
    // Last, after the boost, the equaliser and a crossfade's sum.
    limit(&mut data[..n], &mut output.limiting, device_rate, channels);
    data[n..].fill(0.0);
    let crossed = output.played(frames as u64);
    if let Some(loudness) = crossed {
        shared.set_loudness(loudness);
    }
    output.gain = if n < data.len() {
        // Cut short by the queue running dry, so what comes next fades in.
        0.0
    } else if crossed.is_some() {
        target * next
    } else {
        target
    };
    output.underrun = n < data.len() && output.rate > 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> Output {
        Output {
            rate: 48_000,
            channels: 2,
            base_secs: 30.0,
            next_loudness: 0.5,
            ..Default::default()
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

    fn shared(loudness: f32) -> Shared {
        use std::sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU8, AtomicU32},
        };
        Shared {
            output: Mutex::new(Output {
                samples: vec![1.0; 48].into(),
                gain: loudness,
                ..output()
            }),
            paused: AtomicBool::new(false),
            device_lost: AtomicBool::new(false),
            volume: AtomicU32::new(1f32.to_bits()),
            normalise: AtomicU8::new(crate::audio::Normalise::Normal as u8),
            loudness_gain: AtomicU32::new(loudness.to_bits()),
            download: Mutex::new(None),
            next_download: Mutex::new(None),
            device: Mutex::new(None),
            speed: AtomicU32::new(1f32.to_bits()),
        }
    }

    #[test]
    fn the_queued_track_takes_its_loudness_at_its_first_frame() {
        let shared = shared(0.25);
        shared.output.lock().unwrap().switch_at = Some(3);
        let mut data = [0.0; 12];
        fill(&shared, &mut data, 48_000);
        assert_eq!(data[..6], [0.25; 6], "the current track");
        assert_eq!(data[6..], [0.5; 6], "the next, within the same callback");
        assert_eq!(
            f32::from_bits(shared.loudness_gain.load(Ordering::Relaxed)),
            0.5
        );
        fill(&shared, &mut data, 48_000);
        assert_eq!(data, [0.5; 12], "and on from there");
    }

    #[test]
    fn a_boosted_track_crosses_into_a_cut_one() {
        let shared = shared(1.25);
        let mut out = shared.output.lock().unwrap();
        out.samples = vec![0.25; 48].into();
        out.switch_at = Some(3);
        drop(out);
        let mut data = [0.0; 12];
        fill(&shared, &mut data, 48_000);
        assert_eq!(data[..6], [0.3125; 6], "boosted");
        assert_eq!(data[6..], [0.125; 6], "cut, as it starts");

        let loud = self::shared(1.25);
        fill(&loud, &mut data, 48_000);
        assert_eq!(data, [1.0; 12], "full scale boosted, limited to it");
    }

    #[test]
    fn the_limiter_holds_peaks_and_leaves_the_rest() {
        let mut limiting = 0.0;
        let quiet: Vec<f32> = (0..96).map(|i| (i as f32 * 0.3).sin() * 0.9).collect();
        let mut samples = quiet.clone();
        limit(&mut samples, &mut limiting, 48_000, 3);
        assert_eq!(samples, quiet, "within full scale, untouched");

        let mut loud: Vec<f32> = (0..9_600).map(|i| (i as f32 * 0.3).sin() * 4.0).collect();
        limit(&mut loud, &mut limiting, 48_000, 2);
        assert!(loud.iter().all(|s| s.abs() <= 1.0));
        assert!(loud.iter().any(|s| s.abs() > 0.99), "reaching full scale");
        assert!(limiting > 0.7, "holding it down");

        // A second of quiet lets go entirely, back to leaving it alone.
        let mut rest = vec![0.5; 96_000];
        limit(&mut rest, &mut limiting, 48_000, 2);
        assert_eq!(limiting, 0.0);
        assert_eq!(rest[95_998..], [0.5, 0.5]);
    }

    #[test]
    fn pausing_and_starting_ramp_rather_than_step() {
        let shared = shared(1.0);
        shared.paused.store(true, Ordering::Relaxed);
        let mut data = [0.0; 8];
        fill(&shared, &mut data, 48_000);
        assert_eq!(data, [0.75, 0.75, 0.5, 0.5, 0.25, 0.25, 0.0, 0.0]);
        fill(&shared, &mut data, 48_000);
        assert_eq!(data, [0.0; 8], "silent once faded");
        assert_eq!(
            shared.output.lock().unwrap().samples.len(),
            40,
            "not drained"
        );

        shared.output.lock().unwrap().reset(0.0);
        shared.output.lock().unwrap().samples = vec![1.0; 8].into();
        shared.paused.store(false, Ordering::Relaxed);
        fill(&shared, &mut data, 48_000);
        assert_eq!(
            data,
            [0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0],
            "fades in"
        );
    }

    #[test]
    fn counts_the_queued_track_from_past_its_cut_silence() {
        let mut out = output();
        out.switch_at = Some(0);
        out.next_base_secs = 1.5;
        assert_eq!(out.played(24_000), Some(0.5));
        assert_eq!(out.position(), 2.0);
    }
}
