//! A scored simulation of playback across many setups: speakers that take
//! sound in periods of different lengths, that do or don't tell when, and
//! senders and networks of different steadiness. Run with
//! `cargo test --release --lib sound::push::eval -- --ignored --nocapture`,
//! optionally with `EVAL_SPLIT=train` or `EVAL_SPLIT=test`.
//!
//! Each setup reports the delay sound waits in the speakers (their mean
//! fill over time, which by Little's law is the mean wait) and the gaps,
//! each a take that found less than a period. The score is the mean delay
//! plus 25 ms for each gap per 5 minutes: lower is better.
use super::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Speakers that take `period` frames every `period` frames' time.
struct Sim {
    queue: VecDeque<f32>,
    running: bool,
    started: bool,
    gaps: u32,
    /// Microseconds now, the time of the next take, and the period.
    now: f64,
    next: f64,
    period_us: f64,
    period: usize,
    tells: bool,
}

type Shared = Arc<Mutex<Sim>>;

impl Speaker for Shared {
    fn padding(&mut self) -> Result<usize> {
        Ok(self.lock().unwrap().queue.len() / 2)
    }
    fn capacity(&self) -> usize {
        48_000
    }
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        let mut sim = self.lock().unwrap();
        sim.queue.extend(samples);
        sim.started = true;
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
        let sim = self.lock().unwrap();
        let frames = ((sim.next - sim.now) * 0.048).round() as usize;
        sim.tells.then_some((frames.min(sim.period), sim.period))
    }
}

/// How late packets arrive, beyond the sender's own timing.
#[derive(Clone, Copy)]
enum Network {
    /// Up to this many ms, evenly.
    Even(f64),
    /// Up to `base` ms evenly, and every `every` s on average one packet
    /// `late` ms late.
    Spikes { base: f64, every: f64, late: f64 },
    /// Wireless: an exponential tail of mean `mean` ms, capped at `cap`.
    Tail { mean: f64, cap: f64 },
}

#[derive(Clone, Copy)]
struct Setup {
    name: &'static str,
    /// The speakers' period, and whether they tell when they take sound.
    period_ms: f64,
    tells: bool,
    /// The sender's packet, in frames at its rate, and its rate.
    packet: usize,
    rate: u32,
    /// The sender's clock against the speakers', in parts per million.
    ppm: f64,
    network: Network,
    /// The chance a packet is held back and sent with the next, as a
    /// virtual device that delivers two periods at once now and then.
    doubled: f64,
    /// Where the takes fall against the packets, in ms.
    phase_ms: f64,
}

struct Outcome {
    delay: f64,
    gaps: u32,
    /// Writes to the speakers per second, for the cost.
    writes: f64,
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn packet(start: usize, frames: usize, rate: u32) -> Vec<u8> {
    (start..start + frames)
        .flat_map(|index| {
            let value = ((index as f64 * 440.0 * std::f64::consts::TAU / f64::from(rate)).sin()
                * 8_000.0) as i16;
            let [low, high] = value.to_le_bytes();
            [low, high, low, high]
        })
        .collect()
}

fn run(setup: &Setup, seconds: f64, seed: u64) -> Outcome {
    let period = (setup.period_ms * 48.0).round() as usize;
    let sim: Shared = Arc::new(Mutex::new(Sim {
        queue: VecDeque::new(),
        running: false,
        started: false,
        gaps: 0,
        now: 0.0,
        next: setup.phase_ms * 1000.0,
        period_us: setup.period_ms * 1000.0,
        period,
        tells: setup.tells,
    }));
    let opened = sim.clone();
    let mut push = Push::new(setup.rate, move || Ok(opened.clone()));
    let mut random = Random(seed | 1);
    let packet_us = setup.packet as f64 * 1e6 / f64::from(setup.rate) / (1.0 + setup.ppm * 1e-6);
    // Arrival times of the packets, in order.
    let mut arrivals: VecDeque<(f64, usize)> = VecDeque::new();
    let mut last_arrival = 0.0f64;
    let mut sent = 0usize;
    let mut held = false;
    let total_us = seconds * 1e6;
    let settle_us = 20e6;
    let (mut fill_sum, mut fill_time, mut writes) = (0.0, 0.0, 0u64);
    let mut index = 0usize;
    let step = 100.0;
    let mut now = 0.0f64;
    while now < total_us {
        // The sender records packet `index` at its end.
        let recorded = (index + 1) as f64 * packet_us;
        if recorded <= now + step {
            let late = match setup.network {
                Network::Even(most) => random.next() * most,
                Network::Spikes { base, every, late } => {
                    let spike = random.next() < packet_us / (every * 1e6);
                    random.next() * base + if spike { late } else { 0.0 }
                }
                Network::Tail { mean, cap } => (-mean * (1.0 - random.next()).ln()).min(cap),
            } * 1000.0;
            let mut arrival = (recorded + late).max(last_arrival);
            if held {
                held = false;
            } else if random.next() < setup.doubled {
                held = true;
            }
            if held {
                // Sent with the next one.
                arrival = arrival.max(recorded + packet_us);
            }
            last_arrival = arrival;
            arrivals.push_back((arrival, setup.packet));
            index += 1;
        }
        now += step;
        sim.lock().unwrap().now = now;
        while arrivals.front().is_some_and(|(at, _)| *at <= now) {
            let (_, frames) = arrivals.pop_front().unwrap();
            push.push(&packet(sent, frames, setup.rate));
            sent += frames;
            writes += 1;
        }
        let mut s = sim.lock().unwrap();
        if now >= s.next {
            s.next += s.period_us;
            if s.running {
                let take = (s.period * 2).min(s.queue.len());
                if s.started && take < s.period * 2 && now > 2e6 {
                    s.gaps += 1;
                }
                s.queue.drain(..take);
            }
        }
        if now > settle_us {
            fill_sum += s.queue.len() as f64 / 2.0 / 48.0 * step;
            fill_time += step;
        }
    }
    let gaps = sim.lock().unwrap().gaps;
    // Frames waiting for a whole step wait too.
    let residue = CHUNK as f64 / 2.0 / 48.0;
    Outcome {
        delay: fill_sum / fill_time.max(1.0) + residue,
        gaps,
        writes: writes as f64 / seconds,
    }
}

fn setups() -> Vec<(bool, Setup)> {
    let base = Setup {
        name: "",
        period_ms: 10.0,
        tells: true,
        packet: 480,
        rate: 48_000,
        ppm: 0.0,
        network: Network::Even(0.3),
        doubled: 0.0,
        phase_ms: 3.0,
    };
    // (held out, setup): held-out setups are never looked at while tuning.
    vec![
        (
            false,
            Setup {
                name: "wired, 10 ms speakers, clear phase",
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "wired, 10 ms speakers, takes just after packets",
                phase_ms: 0.4,
                ..base
            },
        ),
        (
            true,
            Setup {
                name: "wired, 10 ms speakers, takes just before packets",
                phase_ms: 9.7,
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "wired, drifting +80 ppm",
                ppm: 80.0,
                ..base
            },
        ),
        (
            true,
            Setup {
                name: "wired, drifting -150 ppm",
                ppm: -150.0,
                phase_ms: 6.0,
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "LAN with a 4 ms spike a minute",
                network: Network::Spikes {
                    base: 1.0,
                    every: 60.0,
                    late: 4.0,
                },
                ppm: 30.0,
                ..base
            },
        ),
        (
            true,
            Setup {
                name: "LAN with a 6 ms spike every 40 s",
                network: Network::Spikes {
                    base: 1.0,
                    every: 40.0,
                    late: 6.0,
                },
                ppm: -40.0,
                phase_ms: 7.0,
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "bursty virtual device",
                doubled: 0.01,
                ppm: 20.0,
                ..base
            },
        ),
        (
            true,
            Setup {
                name: "bursty virtual device, often",
                doubled: 0.05,
                phase_ms: 8.0,
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "wireless",
                network: Network::Tail {
                    mean: 1.5,
                    cap: 25.0,
                },
                ..base
            },
        ),
        (
            true,
            Setup {
                name: "wireless, worse",
                network: Network::Tail {
                    mean: 3.0,
                    cap: 40.0,
                },
                ppm: 60.0,
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "3 ms speakers, wired",
                period_ms: 3.0,
                phase_ms: 1.0,
                ..base
            },
        ),
        (
            true,
            Setup {
                name: "2.67 ms speakers, LAN",
                period_ms: 2.6667,
                network: Network::Even(1.0),
                phase_ms: 0.5,
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "20 ms speakers, LAN",
                period_ms: 20.0,
                network: Network::Even(1.0),
                phase_ms: 12.0,
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "speakers that don't tell, wired",
                tells: false,
                ..base
            },
        ),
        (
            true,
            Setup {
                name: "speakers that don't tell, LAN spikes",
                tells: false,
                network: Network::Spikes {
                    base: 1.0,
                    every: 60.0,
                    late: 4.0,
                },
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "Mac sender, 512-frame packets",
                packet: 512,
                ppm: -60.0,
                ..base
            },
        ),
        (
            true,
            Setup {
                name: "44.1 kHz sender, 441-frame packets",
                packet: 441,
                rate: 44_100,
                network: Network::Even(1.0),
                ..base
            },
        ),
        (
            false,
            Setup {
                name: "20 ms packets",
                packet: 960,
                network: Network::Even(1.0),
                ..base
            },
        ),
    ]
}

#[test]
#[ignore = "runs every setup for minutes of simulated time"]
fn eval() {
    let split = std::env::var("EVAL_SPLIT").unwrap_or_default();
    let seconds: f64 = std::env::var("EVAL_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600.0);
    let mut total = 0.0;
    let mut count = 0;
    for (held_out, setup) in setups() {
        match split.as_str() {
            "train" if held_out => continue,
            "test" if !held_out => continue,
            _ => {}
        }
        let outcome = run(&setup, seconds, 0x9e37_79b9_7f4a_7c15 ^ count as u64);
        let gaps_per_5min = f64::from(outcome.gaps) * 300.0 / seconds;
        let score = outcome.delay + 25.0 * gaps_per_5min;
        total += score;
        count += 1;
        println!(
            "{:5} {:50} delay {:6.2} ms  gaps {:3}  writes/s {:5.0}  score {:6.2}",
            if held_out { "test" } else { "train" },
            setup.name,
            outcome.delay,
            outcome.gaps,
            outcome.writes,
            score
        );
    }
    println!(
        "EVAL {split} mean score {:.2} over {count} setups",
        total / f64::from(count.max(1))
    );
}
