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

/// Frames converted at a time: well under 1 ms at 48 kHz, so little waits for
/// a whole step, and none at all of a 10 ms packet.
const CHUNK: usize = 32;

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

/// When packets arrive against the moments the speakers take sound, and so
/// how many of the speakers' periods each packet needs held for it.
///
/// Each packet's arrival is placed on the speakers' clock: the takes so far
/// (frames written less those still waiting) and the time to the next. Less
/// where packets would arrive were they never late, each a packet's length
/// after the last, that tells how late each packet came against where
/// packets usually arrive, even by more than a period, and wherever packets
/// and periods differ in length. Where packets usually arrive moves as the
/// two clocks drift, minutes from one take to the next, so it is followed
/// packet by packet; how late they come is the network's and the sender's
/// own.
///
/// A packet that usually arrives `s` before its take and comes `late` misses
/// `ceil((late - s) / period)` takes, and the buffer must then still hold
/// that many periods when it should have arrived. So each packet needs as
/// many periods as lateness lately, plus a guard, would make it miss. The
/// worst lateness is rare (some devices deliver a packet late only every
/// minute or two), so the latest of each look (2 s) is remembered for 5
/// minutes, and until 5 minutes have been seen every packet needs a period.
/// Of those looks the second latest counts: lateness that came once in 5
/// minutes, such as one stall of the network, mustn't hold the delay high for
/// 5 minutes. No more periods are held than fit in 40 ms (`EXCESS`), the
/// least the buffer keeps beyond its target before dropping the oldest:
/// later than that is a stall, which running dry handles.
///
/// Running dry can't be measured: while nothing was there to take, the
/// speakers' clock ran on without the frames written less those waiting. So
/// a gap is charged as lateness one period beyond what was measured when it
/// came, however many gaps came before, and like lateness the charge counts
/// only once a second look within 5 minutes has one too; a lone gap is held
/// for by `Pacing` for a few seconds. A gap made by a stall is not charged
/// at all (see `stalled`).
///
/// The guard is 1 ms, as the speakers' own moment of taking sound wavers that
/// much in a way no packet shows. Drift that brings packets closer to their
/// takes counts as time to spare already gone, as much as it takes while the
/// steering brings a period in, but never past the take. Held periods stay
/// while half the guard and the drift again still ask for them, so the delay
/// doesn't swing on a time to spare that wavers about the line.
struct Arrivals {
    /// Where the packets so far would have arrived were they never late, in
    /// the speakers' frames, from where the first did.
    expected: f64,
    /// How far behind `expected` packets usually arrive.
    usual: Option<f64>,
    /// The latest a packet came this look, the lateness charged this look
    /// for running dry, and the seconds of sound it has covered.
    late: f64,
    dry: f64,
    covered: f64,
    /// The look this packet closed holds a charge for running dry that a
    /// stall, piling up the packets just after it, may yet show to be no
    /// lateness.
    charged: bool,
    /// Where packets usually arrived when the drift was last measured, and
    /// the seconds of sound since.
    began: Option<f64>,
    since: f64,
    /// The latest and the charge for running dry of each recent look, newest
    /// first, and how many looks have been seen, up to all of them.
    looks: [(f64, f64); Arrivals::LOOKS],
    seen: usize,
    /// Of the looks remembered: the latest and second latest lateness, and
    /// the largest and second largest charge.
    latest: (f64, f64),
    dried: (f64, f64),
    /// The lateness measured for the last packet, and the seconds of sound
    /// heard, up to 5 minutes.
    measured: f64,
    heard: f64,
    /// How fast packets come later against the speakers' clock, in frames a
    /// second, averaged over about four looks; drift is steady, the noise of
    /// one look isn't.
    drift: Option<f64>,
    /// The speakers' period, once they told it.
    period: Option<f64>,
}

impl Default for Arrivals {
    fn default() -> Self {
        Self {
            expected: 0.0,
            usual: None,
            late: f64::NEG_INFINITY,
            dry: f64::NEG_INFINITY,
            covered: 0.0,
            charged: false,
            began: None,
            since: 0.0,
            looks: [(f64::NEG_INFINITY, f64::NEG_INFINITY); Self::LOOKS],
            seen: 0,
            latest: (f64::NEG_INFINITY, f64::NEG_INFINITY),
            dried: (f64::NEG_INFINITY, f64::NEG_INFINITY),
            measured: 0.0,
            heard: 0.0,
            drift: None,
            period: None,
        }
    }
}

/// What one packet's arrival tells.
struct Arrival {
    /// Periods to hold for it, with the guard and with half the guard (and
    /// the drift) again.
    up: usize,
    down: usize,
    /// Takes it missed by coming later than usual; fewer than none when it
    /// came early enough to catch an earlier take.
    missed: isize,
}

/// The second largest of `top`, the two largest remembered, and `now`.
fn second(top: (f64, f64), now: f64) -> f64 {
    if now >= top.0 { top.0 } else { now.max(top.1) }
}

/// `top`, the two largest so far, with `value` among them.
fn with(top: (f64, f64), value: f64) -> (f64, f64) {
    if value > top.0 {
        (value, top.0)
    } else {
        (top.0, top.1.max(value))
    }
}

impl Arrivals {
    /// Looks remembered, and the sound each covers: 5 minutes.
    const LOOKS: usize = 150;
    const LOOK: f64 = 2.0;
    /// How quickly the usual arrival follows packets that come later than
    /// it, and earlier: it follows the clocks' drift and keeps near the
    /// earliest packets, as lateness only ever delays them.
    const FOLLOW: f64 = 1.0 / 64.0;
    const CATCH: f64 = 1.0 / 8.0;
    /// The frames waiting are read before the speakers' clock, so a take
    /// can fall between: the clock then tells a take just past, which the
    /// frames waiting don't yet show, and the packet seems a period early.
    /// So a packet this close after a take that seems that early is counted
    /// a period later. (A packet can't truly come a period early: those
    /// before it would have had to come a period late.)
    const UNSURE: f64 = 0.001;

    /// Count from the next packet afresh, as when sound starts or the
    /// speakers ran dry: while there was nothing to take, the speakers'
    /// clock ran on without the frames written less those waiting.
    fn restart(&mut self) {
        self.expected = 0.0;
        self.usual = None;
        self.began = None;
    }

    /// The speakers ran dry: a packet came later than the periods held for
    /// it. Charge lateness one period beyond what was measured when it came;
    /// a further gap charges the same, not a period more each time.
    fn ran_dry(&mut self) {
        if let Some(period) = self.period {
            self.dry = self.dry.max(self.measured + period);
        }
        self.restart();
    }

    /// Packets piled up beyond what the buffer keeps before dropping the
    /// oldest (`Pacing::excess`, the target and at least 40 ms more), so the
    /// network stalled for longer than the buffer plus all that: the gap it
    /// made would have come with a period more held too, and with several,
    /// so it is no lateness to hold for. Its charge is dropped, from the
    /// last look closed too if that took it in since.
    fn stalled(&mut self) {
        self.dry = f64::NEG_INFINITY;
        if std::mem::take(&mut self.charged) {
            self.looks[0].1 = f64::NEG_INFINITY;
            self.remember();
        }
        self.restart();
    }

    /// A packet of `frames` (in the speakers' frames), lasting `seconds`,
    /// arrived when the speakers had taken `taken` frames and would take
    /// more `spare` frames later, as they do every `period` frames at `rate`.
    fn saw(
        &mut self,
        taken: u64,
        spare: usize,
        period: usize,
        frames: f64,
        rate: u32,
        seconds: f64,
    ) -> Arrival {
        let period = period.max(1) as f64;
        self.period = Some(period);
        // Only the packet just after a look closed can show its charge was a
        // stall's.
        self.charged = false;
        let spare = (spare as f64).min(period);
        self.expected += frames;
        let mut at = taken as f64 + period - spare - self.expected;
        let late = match self.usual {
            None => {
                self.usual = Some(at);
                0.0
            }
            Some(usual) => {
                let unsure = (f64::from(rate) * Self::UNSURE).min(period / 4.0);
                if period - spare < unsure && at < usual - period / 2.0 {
                    at += period;
                }
                let late = at - usual;
                let follow = if late < 0.0 {
                    Self::CATCH
                } else {
                    Self::FOLLOW
                };
                self.usual = Some(usual + late * follow);
                late
            }
        };
        self.late = self.late.max(late);
        self.covered += seconds;
        self.since += seconds;
        self.heard = (self.heard + seconds).min(Self::LOOKS as f64 * Self::LOOK);
        if self.covered >= Self::LOOK {
            self.look();
        }
        // Had it come as usual: its time to spare, and the takes since.
        let usual_spare = (spare + late).rem_euclid(period);
        let missed = ((spare + late - usual_spare) / period).round() as isize;
        let mut measured = second(self.latest, self.late).max(0.0);
        // Until 5 minutes are seen, every packet needs a period, and packets
        // may yet come later than any so far: the latest of a long-tailed
        // spread grows with the log of how many packets came, so the latest
        // so far is scaled by the log of the packets of 5 minutes over the
        // log of those heard.
        let unseen = self.seen < Self::LOOKS;
        if unseen && seconds > 0.0 {
            let heard = self.heard.max(Self::LOOK) / seconds;
            let all = Self::LOOKS as f64 * Self::LOOK / seconds;
            let latest = self.latest.0.max(self.late);
            measured = measured.max(latest * all.ln() / heard.ln().max(1.0));
        }
        self.measured = measured;
        let most = measured.max(second(self.dried, self.dry));
        // Drift brings the packet this much closer to its take while the
        // steering brings a period in; past the take it would need fewer.
        let lead = self.drift.unwrap_or(0.0).max(0.0) * pacing::SETTLE;
        let floor = f64::from(rate) / 1000.0;
        let most_periods =
            ((f64::from(rate) * pacing::EXCESS.as_secs_f64() / period) as usize).max(1);
        let periods = |scale: f64| {
            let spare = (usual_spare - scale * lead).max(0.0);
            let periods = ((most + scale * floor - spare) / period).ceil().max(0.0) as usize;
            periods.max(usize::from(unseen)).min(most_periods)
        };
        Arrival {
            up: periods(1.0),
            down: periods(1.5),
            missed,
        }
    }

    /// A look ends: remember it, and measure the drift.
    fn look(&mut self) {
        if let Some(usual) = self.usual {
            if let Some(began) = self.began
                && self.since > 0.0
            {
                let drift = (usual - began) / self.since;
                self.drift = Some(self.drift.map_or(drift, |last| last + (drift - last) / 4.0));
            }
            self.began = Some(usual);
            self.since = 0.0;
        }
        let dry = std::mem::replace(&mut self.dry, f64::NEG_INFINITY);
        self.charged = dry > f64::NEG_INFINITY;
        self.looks.rotate_right(1);
        self.looks[0] = (std::mem::replace(&mut self.late, f64::NEG_INFINITY), dry);
        self.seen = (self.seen + 1).min(Self::LOOKS);
        self.covered = 0.0;
        self.remember();
    }

    /// Of the looks remembered, find the two latest and the two largest
    /// charges.
    fn remember(&mut self) {
        let none = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        (self.latest, self.dried) = self
            .looks
            .iter()
            .fold((none, none), |(latest, dried), &(late, dry)| {
                (with(latest, late), with(dried, dry))
            });
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
    /// Frames written to these speakers, for the takes so far.
    written: u64,
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
            written: 0,
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
                    self.written = 0;
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
        let taken = self.written.saturating_sub(padding as u64);
        let target = self.pacing.target();
        let top_up = if !self.streaming {
            // Sound starts: begin at the target, not at whatever is left.
            self.streaming = true;
            self.steering.average = None;
            self.arrivals.restart();
            if let Some(converter) = &mut self.converter {
                converter.reset();
            }
            target.saturating_sub(buffered)
        } else if padding == 0 {
            // The speakers ran out before this arrived.
            self.pacing.ran_dry();
            self.arrivals.ran_dry();
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
                self.arrivals.stalled();
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
            self.written += silence.min(room) as u64;
        }
        self.write_input()?;
        match self.speaker.as_mut().and_then(Speaker::next_take) {
            Some((spare, period)) => {
                // The period in the sender's frames.
                let source_period = (period as f64 / output_per_source).round().max(1.0) as usize;
                self.pacing.takes_known(source_period);
                let arrival = self.arrivals.saw(
                    taken,
                    spare,
                    period,
                    frames as f64 * output_per_source,
                    output_rate,
                    frames as f64 / f64::from(source_rate),
                );
                if top_up == 0 {
                    // Its fill had it come as usual.
                    let fill = buffered as isize + arrival.missed * source_period as isize;
                    self.pacing.need(arrival.up, arrival.down, fill);
                }
            }
            None => {
                self.arrivals = Arrivals::default();
                self.pacing.takes_unknown();
            }
        }
        Ok(())
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
        speaker.write(&self.mapped)?;
        self.written += (self.mapped.len() / channels.max(1)) as u64;
        Ok(())
    }
}

#[cfg(test)]
#[path = "push_eval.rs"]
mod eval;

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
        let (device, push, mean) = run_on(telling(3), 1.0, 0, 420, true);
        assert_eq!(device.lock().unwrap().short, 0);
        let delay = push.pacing().delay();
        assert!(delay < Duration::from_millis(8), "{delay:?}");
        assert!(mean < 12.0, "{mean:.1} ms waiting");
    }

    #[test]
    fn a_packet_late_by_more_than_a_period_needs_as_many_periods() {
        // Packets 5 ms before each take, every 10 ms of the speakers' clock,
        // `late` takes after their own.
        let mut arrivals = Arrivals::default();
        let mut packet = 0u64;
        let mut saw = |arrivals: &mut Arrivals, late: u64| {
            packet += 1;
            arrivals.saw((packet + late) * 480, 240, 480, 480.0, 48_000, 0.01)
        };
        // Long enough to need no period.
        for _ in 0..200 * (Arrivals::LOOKS + 1) {
            assert_eq!(saw(&mut arrivals, 0).missed, 0);
        }
        assert_eq!(saw(&mut arrivals, 0).up, 0);
        // One comes 20 ms late: it missed two takes. One stall alone isn't
        // held for.
        assert_eq!(saw(&mut arrivals, 2).missed, 2);
        assert_eq!(saw(&mut arrivals, 0).up, 0);
        // Another, in another look: from then on each packet needs two.
        for _ in 0..400 {
            saw(&mut arrivals, 0);
        }
        assert_eq!(saw(&mut arrivals, 2).missed, 2);
        let next = saw(&mut arrivals, 0);
        assert_eq!((next.missed, next.up), (0, 2));
    }

    /// Arrivals that have seen 5 minutes of packets 5 ms before each take,
    /// and a way to show them one more, on time.
    fn settled_arrivals() -> (Arrivals, impl FnMut(&mut Arrivals) -> Arrival) {
        let mut arrivals = Arrivals::default();
        let mut packet = 0u64;
        let mut saw = move |arrivals: &mut Arrivals| {
            packet += 1;
            arrivals.saw(packet * 480, 240, 480, 480.0, 48_000, 0.01)
        };
        for _ in 0..200 * (Arrivals::LOOKS + 1) {
            saw(&mut arrivals);
        }
        assert_eq!(saw(&mut arrivals).up, 0);
        (arrivals, saw)
    }

    /// Show packets until the next look begins.
    fn next_look(arrivals: &mut Arrivals, saw: &mut impl FnMut(&mut Arrivals) -> Arrival) {
        arrivals.covered = Arrivals::LOOK;
        saw(arrivals);
    }

    #[test]
    fn speakers_that_stop_telling_their_takes_get_a_packet_in_reserve_at_once() {
        // Packets clear of the takes, long enough to hold no period.
        let (device, mut push, _) = run_on(telling(3), 1.0, 0, 420, true);
        assert!(push.pacing().target() < 480 + 96);
        device.lock().unwrap().tells = false;
        push.push(&tone(0, 480));
        let general = push.pacing().frames_in(Duration::from_millis(4));
        assert!(push.pacing().target() >= 480 + general);
    }

    #[test]
    fn one_gap_alone_is_not_held_for() {
        let (mut arrivals, mut saw) = settled_arrivals();
        arrivals.ran_dry();
        assert_eq!(saw(&mut arrivals).up, 0, "this look");
        next_look(&mut arrivals, &mut saw);
        assert_eq!(saw(&mut arrivals).up, 0, "after it");
    }

    #[test]
    fn gaps_again_hold_one_period_and_no_more() {
        let (mut arrivals, mut saw) = settled_arrivals();
        for gap in 0..6 {
            arrivals.ran_dry();
            saw(&mut arrivals);
            next_look(&mut arrivals, &mut saw);
            let up = saw(&mut arrivals).up;
            assert_eq!(up, usize::from(gap > 0), "after gap {gap}");
        }
        // Forgotten 5 minutes later.
        for _ in 0..Arrivals::LOOKS {
            next_look(&mut arrivals, &mut saw);
        }
        assert_eq!(saw(&mut arrivals).up, 0);
    }

    #[test]
    fn a_stall_is_not_held_for() {
        let (mut arrivals, mut saw) = settled_arrivals();
        for _ in 0..4 {
            arrivals.ran_dry();
            saw(&mut arrivals);
            arrivals.stalled();
            next_look(&mut arrivals, &mut saw);
        }
        assert_eq!(saw(&mut arrivals).up, 0);
        // Also when the packet after the gap closed the look first.
        for _ in 0..4 {
            arrivals.ran_dry();
            next_look(&mut arrivals, &mut saw);
            arrivals.stalled();
            saw(&mut arrivals);
        }
        assert_eq!(saw(&mut arrivals).up, 0);
    }

    #[test]
    fn packets_longer_than_a_period_are_each_expected_where_they_fall() {
        // 512-frame packets against 480-frame takes, on time: each lands
        // elsewhere in the period, and only those close to a take need one.
        let mut arrivals = Arrivals::default();
        let (mut close, mut clear) = (0, 0);
        for packet in 1..=100 * 600u64 {
            let at = packet * 512 + 100;
            let spare = 480 - (at % 480) as usize;
            let arrival = arrivals.saw(at / 480 * 480, spare, 480, 512.0, 48_000, 512.0 / 48_000.0);
            assert_eq!(arrival.missed, 0);
            assert!(arrival.up <= 1);
            if packet > 100 * 400 {
                if arrival.up == 0 {
                    clear += 1;
                } else {
                    close += 1;
                }
            }
        }
        assert!(clear > 0 && close > 0, "{clear} clear, {close} close");
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
            let delay = push.pacing().delay();
            assert!(
                delay >= Duration::from_millis(9),
                "{delay:?} taking at {takes_at}"
            );
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
        // Silence up to the target, then all 100 frames, padded to whole steps.
        assert_eq!(queued, target + 100usize.next_multiple_of(CHUNK));
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
