//! The sample queue the output device drains, and the device callback.

use std::{collections::VecDeque, sync::atomic::Ordering};

use super::Shared;

/// State shared with the device callback.
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
    /// Set by the device callback once the queued track is audible.
    pub(super) advanced: bool,
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

/// Device callback: drains the queue, or writes silence while paused.
pub(super) fn fill(shared: &Shared, data: &mut [f32]) {
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
}
