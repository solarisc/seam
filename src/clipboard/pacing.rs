use crate::{input::Controller, status::Stats};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

const BURST_BYTES: usize = 64 * 1024;
const ACTIVITY_WINDOW: Duration = Duration::from_millis(500);
const MAX_PAUSE_US: u64 = 20_000;

#[derive(Clone)]
pub(crate) struct InputSignals {
    controller: Arc<Controller>,
    stats: Arc<Stats>,
}

impl InputSignals {
    pub(super) fn peer_name(&self) -> String {
        self.stats.peer_name.read().unwrap().clone()
    }
    pub(crate) fn new(controller: Arc<Controller>, stats: Arc<Stats>) -> Self {
        Self { controller, stats }
    }
}

pub(super) struct BulkPacer {
    signals: InputSignals,
    sent: u64,
    received: u64,
    last_activity: Option<Instant>,
    last_rtt_sample: u64,
    pause_us: u64,
    bytes_since_wait: usize,
    #[cfg(windows)]
    timer: TimerState,
}

impl BulkPacer {
    pub(super) fn new(signals: InputSignals) -> Self {
        Self {
            sent: signals.controller.sent_events.load(Ordering::Relaxed),
            received: signals.stats.received_events.load(Ordering::Relaxed),
            last_rtt_sample: signals.stats.rtt_samples.load(Ordering::Acquire),
            signals,
            last_activity: None,
            pause_us: 0,
            bytes_since_wait: 0,
            #[cfg(windows)]
            timer: TimerState::Uninitialized,
        }
    }

    pub(super) fn after_chunk(&mut self, bytes: usize) {
        self.bytes_since_wait += bytes;
        if self.bytes_since_wait < BURST_BYTES {
            return;
        }
        self.bytes_since_wait %= BURST_BYTES;

        let sent = self.signals.controller.sent_events.load(Ordering::Relaxed);
        let received = self.signals.stats.received_events.load(Ordering::Relaxed);
        if sent != self.sent || received != self.received {
            self.last_activity = Some(Instant::now());
        }
        self.sent = sent;
        self.received = received;
        if self
            .last_activity
            .is_none_or(|last| last.elapsed() >= ACTIVITY_WINDOW)
        {
            return;
        }

        // The input heartbeat supplies a route-specific floor. It is sampled
        // only every 250 ms; bulk sends never add work to the input threads.
        let floor = self.signals.stats.best_rtt_us.load(Ordering::Relaxed);
        if floor == u64::MAX || floor == 0 {
            return;
        }
        let seed = (floor / 4).clamp(100, 2_000);
        self.pause_us = self.pause_us.max(seed);
        let sample = self.signals.stats.rtt_samples.load(Ordering::Acquire);
        if sample != self.last_rtt_sample {
            self.last_rtt_sample = sample;
            let current = self.signals.stats.rtt_us.load(Ordering::Relaxed);
            let elevated =
                current != u64::MAX && current > floor.saturating_add((floor / 2).max(1_000));
            self.pause_us = if elevated {
                self.pause_us.saturating_mul(2).min(MAX_PAUSE_US)
            } else {
                (self.pause_us * 3 / 4).max(seed)
            };
        }
        self.wait(Duration::from_micros(self.pause_us));
    }

    fn wait(&mut self, duration: Duration) {
        #[cfg(windows)]
        {
            if matches!(self.timer, TimerState::Uninitialized) {
                self.timer = WaitTimer::new()
                    .map(TimerState::Ready)
                    .unwrap_or(TimerState::Unavailable);
            }
            if let TimerState::Ready(timer) = &self.timer
                && timer.wait(duration)
            {
                return;
            }
            self.timer = TimerState::Unavailable;
        }
        std::thread::sleep(duration);
    }
}

#[cfg(windows)]
enum TimerState {
    Uninitialized,
    Ready(WaitTimer),
    Unavailable,
}

#[cfg(windows)]
struct WaitTimer(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WaitTimer {
    fn new() -> Option<Self> {
        use windows_sys::Win32::System::Threading::{
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, TIMER_ALL_ACCESS,
        };
        let handle = unsafe {
            CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        let handle = if handle.is_null() {
            log::warn!("high-resolution transfer timer unavailable; using a regular timer");
            unsafe {
                CreateWaitableTimerExW(std::ptr::null(), std::ptr::null(), 0, TIMER_ALL_ACCESS)
            }
        } else {
            handle
        };
        if handle.is_null() {
            log::warn!("transfer timer unavailable; using the system sleep");
            None
        } else {
            Some(Self(handle))
        }
    }

    fn wait(&self, duration: Duration) -> bool {
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0,
            System::Threading::{SetWaitableTimer, WaitForSingleObject},
        };
        let due = -(duration.as_micros().min(i64::MAX as u128 / 10) as i64 * 10);
        unsafe {
            SetWaitableTimer(self.0, &due, 0, None, std::ptr::null(), 0) != 0
                && WaitForSingleObject(self.0, 100) == WAIT_OBJECT_0
        }
    }
}

#[cfg(windows)]
impl Drop for WaitTimer {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}
