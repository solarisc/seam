//! Mixes the sound of devices recorded at once, as Windows records each
//! device that plays.
//!
//! One device leads: its sound goes out the moment it arrives. Each other
//! device's sound waits just long enough to be there whenever the leader's
//! arrives, is matched to the leader's clock, which drifts from its own, and
//! is added in.
// Windows mixes this way; elsewhere it is built only to be tested.
#![cfg_attr(not(windows), allow(dead_code))]
use super::pacing::{self, Steering};
use anyhow::Result;
use rubato::{Adjustable, FixedAsync, audioadapter_buffers::direct::InterleavedSlice};

/// Frames matched to the leader's clock at a time.
const CHUNK: usize = 64;

/// A device's recording arrives every 10 ms: this many frames at 48 kHz.
pub(super) const PACKET: usize = 480;

/// Room for a packet that arrives a little late: 2 ms at 48 kHz.
const MARGIN: usize = 96;

/// A following device's sound held this far beyond its target is cut back to
/// it, as after the leader stalled.
const EXCESS: usize = 2 * PACKET;

/// A following device's sound, waiting to be mixed into the leader's.
pub(super) struct Follower {
    rate: u32,
    /// As recorded: interleaved stereo.
    waiting: Vec<f32>,
    /// Matched to the leader's clock.
    matched: Vec<f32>,
    converter: Box<dyn Adjustable<f32>>,
    steering: Steering,
    step: Vec<f32>,
    /// Mixed in. After running short, only again once enough has arrived.
    flowing: bool,
}

impl Follower {
    pub(super) fn new(rate: u32) -> Result<Self> {
        Ok(Self {
            rate,
            waiting: Vec::new(),
            matched: Vec::new(),
            converter: pacing::converter(rate, rate, CHUNK, FixedAsync::Input)?,
            steering: Steering::default(),
            step: Vec::new(),
            flowing: false,
        })
    }

    /// Where this device's recorded sound goes as it arrives.
    pub(super) fn waiting(&mut self) -> &mut Vec<f32> {
        &mut self.waiting
    }

    /// Hand over everything waiting, as when this device begins to lead.
    pub(super) fn take(&mut self, out: &mut Vec<f32>) {
        if self.matched.is_empty() && self.waiting.is_empty() {
            return;
        }
        out.append(&mut self.matched);
        out.append(&mut self.waiting);
        self.restart();
    }

    /// Start over, as when this device begins to follow.
    pub(super) fn restart(&mut self) {
        self.waiting.clear();
        self.matched.clear();
        self.converter.reset();
        self.steering.average = None;
        self.flowing = false;
    }

    /// Add this device's sound to the leader's `out`, which just arrived.
    /// True if it ran short.
    pub(super) fn mix_into(&mut self, out: &mut [f32]) -> Result<bool> {
        self.convert()?;
        let frames = out.len() / 2;
        // Before each of the leader's packets: as much again, one packet of
        // this device's own that may arrive just after, and a margin.
        let target = frames + PACKET + MARGIN;
        let level = |follower: &Self| (follower.matched.len() + follower.waiting.len()) / 2;
        if !self.flowing {
            if level(self) < target {
                return Ok(false);
            }
            self.flowing = true;
        }
        if level(self) > target + EXCESS {
            let cut = (level(self) - target).min(self.matched.len() / 2);
            self.matched.drain(..cut * 2);
            self.steering.average = None;
        }
        let ratio = self.steering.ratio(level(self), target, frames, self.rate);
        let _ = self.converter.set_resample_ratio_relative(ratio, true);
        let take = frames.min(self.matched.len() / 2);
        for (sample, added) in out.iter_mut().zip(self.matched.drain(..take * 2)) {
            *sample += added;
        }
        let short = take < frames;
        if short {
            self.flowing = false;
            self.steering.average = None;
        }
        Ok(short)
    }

    /// Match every whole step waiting to the leader's clock.
    fn convert(&mut self) -> Result<()> {
        let steps = self.waiting.len() / 2 / CHUNK;
        if steps == 0 {
            return Ok(());
        }
        let most = self.converter.output_frames_max();
        self.step.resize(most * 2, 0.0);
        for step in 0..steps {
            let samples = &self.waiting[step * CHUNK * 2..(step + 1) * CHUNK * 2];
            let input = InterleavedSlice::new(samples, 2, CHUNK)?;
            let mut output = InterleavedSlice::new_mut(&mut self.step, 2, most)?;
            let (_, written) = self
                .converter
                .process_into_buffer(&input, &mut output, None)?;
            self.matched.extend_from_slice(&self.step[..written * 2]);
        }
        self.waiting.drain(..steps * CHUNK * 2);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a leader and a follower for `seconds`, the follower's clock
    /// `ppm` parts per million fast and its packets `offset` ms after the
    /// leader's. The follower plays a constant 0.25, the leader silence.
    /// Returns the mixed sound, and how often the follower ran short.
    fn run(seconds: f64, ppm: f64, offset: f64) -> (Vec<f32>, usize) {
        let mut follower = Follower::new(48_000).unwrap();
        let mut mixed = Vec::new();
        let mut short = 0;
        let (mut leader_at, mut follower_at, mut owed) = (0.0, offset / 1e3, 0.0);
        let period = 0.01;
        while leader_at < seconds {
            if follower_at < leader_at {
                // Every 10 ms, by the leader's clock, a little more or less.
                owed += PACKET as f64 * (1.0 + ppm / 1e6);
                let frames = owed as usize;
                owed -= frames as f64;
                follower
                    .waiting()
                    .extend(std::iter::repeat_n(0.25, frames * 2));
                follower_at += period;
            } else {
                let mut out = vec![0.0; PACKET * 2];
                if follower.mix_into(&mut out).unwrap() {
                    short += 1;
                }
                mixed.extend_from_slice(&out);
                leader_at += period;
            }
        }
        (mixed, short)
    }

    #[test]
    fn a_following_device_is_mixed_in_without_gaps() {
        for offset in [0.0, 3.0, 9.9] {
            let (mixed, short) = run(20.0, 0.0, offset);
            assert_eq!(short, 0, "offset {offset}");
            // Silent only until enough has arrived.
            let start = mixed.iter().position(|sample| *sample > 0.0).unwrap();
            assert!(start <= 3 * PACKET * 2, "starts at {start}");
            assert!(
                mixed[start..]
                    .iter()
                    .all(|sample| (sample - 0.25).abs() < 1e-3)
            );
        }
    }

    #[test]
    fn a_following_clock_that_drifts_is_matched() {
        for ppm in [-300.0, 300.0] {
            let (_, short) = run(120.0, ppm, 5.0);
            assert_eq!(short, 0, "{ppm} ppm");
        }
    }

    #[test]
    fn a_device_that_begins_to_lead_hands_over_what_waits() {
        let mut follower = Follower::new(48_000).unwrap();
        follower.waiting().extend_from_slice(&[0.5; 200]);
        let mut out = Vec::new();
        follower.take(&mut out);
        assert_eq!(out.len(), 200);
        assert!(follower.waiting().is_empty());
    }
}
