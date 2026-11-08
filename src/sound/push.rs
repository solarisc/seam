//! Plays sound by writing it into the speakers' own buffer the moment it
//! arrives, so that buffer is the only one it waits in.
//!
//! The speakers' buffer is kept just full enough to ride out the network's
//! timing (see `pacing`): when sound starts it is topped up with silence to
//! that level, and from then on each packet goes straight in. Nothing wakes
//! up on a timer to feed the speakers; playing costs one write per packet.
// Windows plays this way; elsewhere it is built only to be tested.
#![cfg_attr(not(windows), allow(dead_code))]
use super::{
    dsp,
    pacing::{self, Pacing, Steering},
};
use anyhow::Result;
use rubato::{Adjustable, FixedAsync, audioadapter_buffers::direct::InterleavedSlice};
use std::time::Instant;

/// Frames converted at a time: under 1.5 ms at 48 kHz, so little waits for a
/// whole step.
const CHUNK: usize = 64;

/// The speakers' buffer, which plays on by itself and plays silence when it
/// runs out.
pub(super) trait Speaker {
    /// Frames written and not yet played.
    fn padding(&mut self) -> Result<usize>;
    /// The most frames the buffer holds.
    fn capacity(&self) -> usize;
    /// Append interleaved frames of `channels()` channels.
    fn write(&mut self, samples: &[f32]) -> Result<()>;
    fn channels(&self) -> usize;
    fn rate(&self) -> u32;
    fn running(&self) -> bool;
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    /// Frames until the speakers next take sound from the buffer, and the
    /// frames between takes; none while that isn't known.
    fn next_take(&mut self) -> Option<(usize, usize)> {
        None
    }
}

/// How close packets come to the speakers taking sound. A packet that
/// arrives just before a take misses it when it comes a little late, so only
/// then must a packet be held in reserve. Holding starts as soon as a packet
/// comes within 2 ms of a take, and ends once every packet for 30 s has kept
/// more than 3 ms clear, so the delay doesn't swing back and forth.
#[derive(Default)]
struct Arrivals {
    /// The least time to spare this look, and in each recent look.
    closest: Option<usize>,
    looks: [Option<usize>; 15],
    seen: usize,
    clear: bool,
}

impl Arrivals {
    /// Packets seen per look: 2 s of 10 ms packets.
    const PER_LOOK: usize = 200;

    /// A packet arrived `spare` frames before the speakers take sound, at
    /// `rate`. True while no packet need be held in reserve.
    fn saw(&mut self, spare: usize, rate: u32) -> bool {
        let closest = self.closest.map_or(spare, |closest| closest.min(spare));
        self.closest = Some(closest);
        self.seen += 1;
        if self.seen >= Self::PER_LOOK {
            self.looks.rotate_right(1);
            self.looks[0] = self.closest.take();
            self.seen = 0;
        }
        let (near, far) = (rate as usize / 500, rate as usize / 333);
        if self.clear {
            self.clear = closest >= near;
        } else {
            self.clear = closest > far
                && self
                    .looks
                    .iter()
                    .all(|look| look.is_some_and(|spare| spare > far));
        }
        self.clear
    }
}

/// One controlled computer's sound, written into the speakers as it arrives.
pub(super) struct Push<S> {
    speaker: Option<S>,
    open: Box<dyn FnMut() -> Result<S> + Send>,
    pacing: Pacing,
    arrivals: Arrivals,
    steering: Steering,
    converter: Option<Box<dyn Adjustable<f32>>>,
    /// The sender's and the speakers' rates the converter was made for.
    rates: (u32, u32),
    /// Stereo frames at the sender's rate, short of a whole step.
    input: Vec<f32>,
    /// One converted step, all converted steps, and those in the speakers'
    /// channels.
    step: Vec<f32>,
    converted: Vec<f32>,
    mapped: Vec<f32>,
    /// Sound is streaming in: since the last pause, written without a gap.
    streaming: bool,
    /// The speakers started, or failed to: the player should look.
    woke: bool,
    /// Packets received, so the player can tell sound is arriving.
    pub(super) received: u64,
    /// The speakers failed to open; not before then again.
    retry: Option<Instant>,
    /// Why the speakers can't play, for the window.
    pub(super) problem: Option<String>,
}

impl<S: Speaker> Push<S> {
    pub(super) fn new(rate: u32, open: impl FnMut() -> Result<S> + Send + 'static) -> Self {
        Self {
            speaker: None,
            open: Box::new(open),
            pacing: Pacing::new(rate),
            arrivals: Arrivals::default(),
            steering: Steering::default(),
            converter: None,
            rates: (0, 0),
            input: Vec::new(),
            step: Vec::new(),
            converted: Vec::new(),
            mapped: Vec::new(),
            streaming: false,
            woke: false,
            received: 0,
            retry: None,
            problem: None,
        }
    }

    pub(super) fn pacing(&self) -> &Pacing {
        &self.pacing
    }

    pub(super) fn speaker(&mut self) -> Option<&mut S> {
        self.speaker.as_mut()
    }

    /// Close the speakers; the next sound opens them again.
    pub(super) fn close(&mut self) {
        self.speaker = None;
        self.streaming = false;
    }

    /// Sound follows at `rate`. A new rate starts afresh.
    pub(super) fn set_rate(&mut self, rate: u32) {
        if rate.max(1) != self.pacing.rate() {
            self.pacing = Pacing::new(rate);
            self.input.clear();
            self.streaming = false;
        }
    }

    /// Interleaved 16-bit stereo, little-endian: four bytes a frame.
    pub(super) fn push(&mut self, samples: &[u8]) {
        self.received += 1;
        let frames = samples.len() / 4;
        self.pacing.packet(frames);
        let before = self.input.len();
        dsp::to_float(&samples[..frames * 4], &mut self.input);
        if let Err(error) = self.play(frames, before) {
            // The speakers stopped working; the next sound opens them again.
            log::info!("the speakers stopped working: {error:#}");
            self.close();
            self.input.clear();
        }
    }

    /// The sender stopped because nothing plays: finish what's left.
    pub(super) fn pause(&mut self) {
        if self.streaming && !self.input.is_empty() {
            // Pad the last step with silence so all of it plays.
            let missing = (CHUNK - self.input.len() / 2 % CHUNK) % CHUNK;
            self.input.resize(self.input.len() + missing * 2, 0.0);
            if let Err(error) = self.write_input() {
                log::info!("the speakers stopped working: {error:#}");
                self.close();
            }
        }
        self.input.clear();
        self.streaming = false;
    }

    /// The speakers started, or failed to, since the last call: the player
    /// should look at them.
    pub(super) fn wants_speakers(&mut self) -> bool {
        std::mem::take(&mut self.woke)
    }

    /// Speakers to write to, opened or resumed as needed.
    fn ready_speaker(&mut self) -> Result<Option<&mut S>> {
        if self.speaker.is_none() {
            let now = Instant::now();
            if self.retry.is_some_and(|retry| now < retry) {
                return Ok(None);
            }
            match (self.open)() {
                Ok(speaker) => {
                    self.speaker = Some(speaker);
                    self.retry = None;
                    self.problem = None;
                    self.streaming = false;
                    self.woke = true;
                }
                Err(error) => {
                    if self.problem.is_none() {
                        log::warn!("cannot play sound here: {error:#}");
                    }
                    self.problem = Some(format!("Can't play sound on this computer: {error}"));
                    self.retry = Some(now + pacing::REOPEN);
                    self.woke = true;
                    return Ok(None);
                }
            }
        }
        let speaker = self.speaker.as_mut().expect("opened above");
        if !speaker.running() {
            speaker.start()?;
            self.streaming = false;
            self.woke = true;
        }
        Ok(Some(speaker))
    }

    /// Write the packet of `frames` just appended after `before` samples.
    fn play(&mut self, frames: usize, before: usize) -> Result<()> {
        let source_rate = self.pacing.rate();
        let Some(speaker) = self.ready_speaker()? else {
            self.input.clear();
            return Ok(());
        };
        let (output_rate, padding) = (speaker.rate(), speaker.padding()?);
        let output_per_source = f64::from(output_rate) / f64::from(source_rate);
        if self.rates != (source_rate, output_rate) || self.converter.is_none() {
            self.converter = Some(pacing::converter(
                source_rate,
                output_rate,
                CHUNK,
                FixedAsync::Input,
            )?);
            self.rates = (source_rate, output_rate);
        }
        // Buffered before this packet, in the sender's frames: in the
        // speakers, and waiting for a whole step.
        let buffered = (padding as f64 / output_per_source) as usize + before / 2;
        let target = self.pacing.target();
        let top_up = if !self.streaming {
            // Sound starts: begin at the target, not at whatever is left.
            self.streaming = true;
            self.steering.average = None;
            if let Some(converter) = &mut self.converter {
                converter.reset();
            }
            target.saturating_sub(buffered)
        } else if padding == 0 {
            // The speakers ran out before this arrived.
            self.pacing.ran_dry();
            self.arrivals = Arrivals::default();
            self.pacing.target().saturating_sub(buffered)
        } else {
            self.pacing.low_point(buffered);
            self.pacing.played(frames, buffered);
            let ratio = self.steering.ratio(buffered, target, frames, source_rate);
            if let Some(converter) = &mut self.converter {
                let _ = converter.set_resample_ratio_relative(ratio, true);
            }
            if buffered > self.pacing.excess() {
                // Far behind, as after the network held packets back: skip
                // this packet rather than play late from now on.
                self.input.truncate(before);
                return Ok(());
            }
            0
        };
        if top_up > 0 {
            let silence = (top_up as f64 * output_per_source) as usize;
            let speaker = self.speaker.as_mut().expect("ready above");
            let room = speaker.capacity().saturating_sub(padding);
            let channels = speaker.channels();
            self.mapped.clear();
            self.mapped.resize(silence.min(room) * channels, 0.0);
            speaker.write(&self.mapped)?;
        }
        self.write_input()?;
        let output_rate = self.speakers_rate();
        let clear = match self.speaker.as_mut().and_then(Speaker::next_take) {
            Some((spare, _)) => {
                self.pacing.takes_known();
                self.arrivals.saw(spare, output_rate)
            }
            None => {
                self.arrivals = Arrivals::default();
                false
            }
        };
        self.pacing.hold_reserve(!clear);
        Ok(())
    }

    fn speakers_rate(&self) -> u32 {
        self.speaker.as_ref().map_or(48_000, Speaker::rate)
    }

    /// Convert every whole step waiting and write it to the speakers.
    fn write_input(&mut self) -> Result<()> {
        let (Some(converter), Some(speaker)) = (&mut self.converter, &mut self.speaker) else {
            return Ok(());
        };
        let steps = self.input.len() / 2 / CHUNK;
        if steps == 0 {
            return Ok(());
        }
        let most = converter.output_frames_max();
        self.step.resize(most * 2, 0.0);
        self.converted.clear();
        for step in 0..steps {
            let samples = &self.input[step * CHUNK * 2..(step + 1) * CHUNK * 2];
            let input = InterleavedSlice::new(samples, 2, CHUNK)?;
            let mut output = InterleavedSlice::new_mut(&mut self.step, 2, most)?;
            let (_, written) = converter.process_into_buffer(&input, &mut output, None)?;
            self.converted.extend_from_slice(&self.step[..written * 2]);
        }
        self.input.drain(..steps * CHUNK * 2);
        let channels = speaker.channels();
        let frames = self.converted.len() / 2;
        let room = speaker.capacity().saturating_sub(speaker.padding()?);
        self.mapped.clear();
        self.mapped.reserve(frames * channels);
        for &[left, right] in self.converted.as_chunks::<2>().0.iter().take(room) {
            match channels {
                1 => self.mapped.push((left + right) * 0.5),
                _ => {
                    self.mapped.extend_from_slice(&[left, right]);
                    self.mapped.extend(std::iter::repeat_n(0.0, channels - 2));
                }
            }
        }
        speaker.write(&self.mapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    /// A speaker that plays a 10 ms period at a time, as Windows' shared
    /// mode does, and counts the times it found less than a period.
    struct Fake {
        queue: VecDeque<f32>,
        played: Vec<f32>,
        running: bool,
        /// Sound has been written, so from now on it streams without a gap.
        started: bool,
        short: u32,
        /// The time in ms, the ms of each 10 at which it takes sound, and
        /// whether it tells when that is.
        now: usize,
        takes_at: usize,
        tells: bool,
    }

    impl Fake {
        fn tick(&mut self) {
            if !self.running {
                return;
            }
            let period = 480 * 2;
            if self.started && self.queue.len() < period {
                self.short += 1;
            }
            let take = period.min(self.queue.len());
            self.played.extend(self.queue.drain(..take));
            self.played.extend(std::iter::repeat_n(0.0, period - take));
        }
    }

    impl Speaker for std::sync::Arc<std::sync::Mutex<Fake>> {
        fn padding(&mut self) -> Result<usize> {
            Ok(self.lock().unwrap().queue.len() / 2)
        }
        fn capacity(&self) -> usize {
            9_600
        }
        fn write(&mut self, samples: &[f32]) -> Result<()> {
            let mut fake = self.lock().unwrap();
            fake.queue.extend(samples);
            fake.started = true;
            Ok(())
        }
        fn channels(&self) -> usize {
            2
        }
        fn rate(&self) -> u32 {
            48_000
        }
        fn running(&self) -> bool {
            self.lock().unwrap().running
        }
        fn start(&mut self) -> Result<()> {
            self.lock().unwrap().running = true;
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            self.lock().unwrap().running = false;
            Ok(())
        }
        fn next_take(&mut self) -> Option<(usize, usize)> {
            let fake = self.lock().unwrap();
            let ms = (fake.takes_at + 10 - fake.now % 10) % 10;
            fake.tells.then_some((ms * 48, 480))
        }
    }

    type Shared = std::sync::Arc<std::sync::Mutex<Fake>>;

    fn speaker() -> Shared {
        std::sync::Arc::new(std::sync::Mutex::new(Fake {
            queue: VecDeque::new(),
            played: Vec::new(),
            running: false,
            started: false,
            short: 0,
            now: 0,
            takes_at: 3,
            tells: false,
        }))
    }

    fn tone(start: usize, frames: usize) -> Vec<u8> {
        (start..start + frames)
            .flat_map(|index| {
                let value = ((index as f64 * 1000.0 * std::f64::consts::TAU / 48_000.0).sin()
                    * 16_000.0) as i16;
                let [low, high] = value.to_le_bytes();
                [low, high, low, high]
            })
            .collect()
    }

    /// Run for `seconds`: 10 ms packets on the sender's clock, each up to
    /// `lateness` ms late, and the speakers playing a period every 10 ms.
    /// Returns the mean frames waiting in the speakers.
    fn run(clock: f64, lateness: usize, seconds: usize) -> (Shared, Push<Shared>, f64) {
        run_on(speaker(), clock, lateness, seconds, false)
    }

    fn run_on(
        device: Shared,
        clock: f64,
        lateness: usize,
        seconds: usize,
        steady: bool,
    ) -> (Shared, Push<Shared>, f64) {
        let opened = device.clone();
        let mut push = Push::new(48_000, move || Ok(opened.clone()));
        let (mut sent, mut random) = (0usize, 0x2468_ace1u32);
        let mut in_flight: VecDeque<(usize, Vec<u8>)> = VecDeque::new();
        let (mut sum, mut count) = (0.0, 0.0);
        for step in 0..seconds * 1000 {
            device.lock().unwrap().now = step;
            let due = if steady {
                // Whole packets on the sender's clock, as a device records.
                let packets = ((step + 1) as f64 * clock / 10.0) as usize;
                packets * 480
            } else {
                ((step + 1) as f64 * 48.0 * clock) as usize
            };
            if due >= sent + 480 {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                let delay = random as usize % (lateness + 1);
                let arrives = in_flight
                    .back()
                    .map_or(step + delay, |(last, _)| (*last).max(step + delay));
                in_flight.push_back((arrives, tone(sent, due - sent)));
                sent = due;
            }
            while in_flight.front().is_some_and(|(at, _)| *at <= step) {
                push.push(&in_flight.pop_front().unwrap().1);
            }
            let takes_at = device.lock().unwrap().takes_at;
            if step % 10 == takes_at {
                device.lock().unwrap().tick();
            }
            if step > seconds * 500 {
                sum += device.lock().unwrap().queue.len() as f64 / 2.0;
                count += 1.0;
            }
        }
        (device, push, sum / count / 48.0)
    }

    #[test]
    fn sound_waits_only_as_long_as_the_network_needs() {
        // Clocks slower, equal and faster, on a steady network and on one
        // that holds packets back up to 15 ms. Everything waiting is counted,
        // the speakers' own buffer included.
        for (clock, lateness, most) in [
            (0.9997, 0, 26.0),
            (1.0, 0, 20.0),
            (1.0003, 0, 20.0),
            (1.0001, 3, 20.0),
            (1.0001, 15, 28.0),
        ] {
            let (device, _, mean) = run(clock, lateness, 300);
            let short = device.lock().unwrap().short;
            assert_eq!(short, 0, "ran short at clock {clock}, {lateness} ms late");
            assert!(
                mean < most,
                "{mean:.1} ms waiting at clock {clock}, {lateness} ms late"
            );
        }
    }

    /// Speakers that tell when they take sound, at `takes_at` ms of each 10.
    fn telling(takes_at: usize) -> Shared {
        let device = speaker();
        {
            let mut fake = device.lock().unwrap();
            fake.takes_at = takes_at;
            fake.tells = true;
        }
        device
    }

    #[test]
    fn packets_clear_of_the_takes_need_no_packet_in_reserve() {
        // Packets arrive 4 ms before the speakers take sound.
        let (device, push, mean) = run_on(telling(3), 1.0, 0, 120, true);
        assert_eq!(device.lock().unwrap().short, 0);
        let delay = push.pacing().delay();
        assert!(delay < Duration::from_millis(8), "{delay:?}");
        assert!(mean < 12.0, "{mean:.1} ms waiting");
    }

    #[test]
    fn packets_close_to_the_takes_keep_a_packet_in_reserve() {
        // Packets arrive just before the speakers take sound, now and then
        // 1 ms late.
        for takes_at in [9, 0] {
            let (device, push, _) = run_on(telling(takes_at), 1.0, 1, 60, true);
            assert_eq!(
                device.lock().unwrap().short,
                0,
                "ran short taking at {takes_at}"
            );
            assert!(push.pacing().delay() >= Duration::from_millis(10));
        }
    }

    #[test]
    fn a_reserve_returns_before_drifting_packets_reach_the_takes() {
        // 100 ppm moves the packets 1 ms against the takes every 10 s, past
        // them more than once in four minutes, now and then 1 ms late.
        for clock in [0.9999, 1.0001] {
            let (device, _, _) = run_on(telling(3), clock, 1, 240, true);
            assert_eq!(
                device.lock().unwrap().short,
                0,
                "ran short at clock {clock}"
            );
        }
    }
    #[test]
    fn sound_starts_at_the_target_and_plays_out_in_full() {
        let device = speaker();
        let opened = device.clone();
        let mut push = Push::new(48_000, move || Ok(opened.clone()));
        // A sound shorter than one packet, then a pause.
        push.push(&tone(0, 100));
        push.pause();
        let target = push.pacing().target();
        let queued = device.lock().unwrap().queue.len() / 2;
        // Silence up to the target, then all 100 frames, padded to two steps.
        assert_eq!(queued, target + 2 * CHUNK);
        let samples: Vec<f32> = device.lock().unwrap().queue.iter().copied().collect();
        let heard = samples.iter().filter(|sample| sample.abs() > 0.0).count();
        assert!(heard >= 2 * 90, "{heard}");
    }

    #[test]
    fn the_same_rate_writes_the_samples_unchanged() {
        let device = speaker();
        let opened = device.clone();
        let mut push = Push::new(48_000, move || Ok(opened.clone()));
        let sent = tone(0, 480);
        push.push(&sent);
        let target = push.pacing().target();
        let queue: Vec<f32> = device.lock().unwrap().queue.iter().copied().collect();
        let written = &queue[target * 2..];
        for (sample, &pair) in written.iter().step_by(2).zip(sent.as_chunks::<4>().0) {
            let original = f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32_768.0;
            assert!((sample - original).abs() < 1e-6);
        }
    }
}
