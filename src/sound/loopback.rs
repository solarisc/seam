//! Windows: records what this computer plays, on whichever speakers it plays.
//!
//! Apps can each pick their own speakers, and recording one device's mix
//! (loopback) misses the others. So each device that plays is recorded,
//! directly, which adds no delay, and the recordings are mixed (see `mix`).
//! Each device's level meter shows which play. A device stays recorded for a
//! while after it last played, so its next sound starts on time.
//!
//! Windows can also record every app at once, wherever it plays (process
//! loopback), but holds that sound back by tens of milliseconds while it
//! combines the devices.
use super::{capture::Parts, dsp, mix::Follower, wasapi};
use anyhow::{Context, Result};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
    Foundation::{CloseHandle, HANDLE, PROPERTYKEY, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Media::{
        Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE,
            DEVICE_STATE_ACTIVE, EDataFlow, ERole, Endpoints::IAudioMeterInformation,
            IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
            IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, WAVEFORMATEX,
            eConsole, eRender,
        },
        Multimedia::WAVE_FORMAT_IEEE_FLOAT,
    },
    System::{
        Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ},
        Threading::{CreateEventW, SetEvent, WaitForMultipleObjects},
    },
};
use windows_core::{PCWSTR, implement};

/// The format asked for; Windows converts every recording to it.
pub(super) const RATE: u32 = 48_000;

/// How often the level meters are read: the longest a sound goes unrecorded
/// when it starts on a device not recorded already.
const LOOK: Duration = Duration::from_millis(100);

/// A device that played this recently stays recorded.
const LINGER: Duration = Duration::from_secs(60);

/// The leading device has stopped when nothing has come from it for this
/// long while another device's sound arrives.
const STALL: Duration = Duration::from_millis(30);

/// 20 ms of room, in 100 ns units; sound is taken as each 10 ms arrives.
const ROOM: i64 = 200_000;

/// One wait covers at most 64 handles, one of them the stop.
const MOST: usize = 63;

const FORMAT: WAVEFORMATEX = WAVEFORMATEX {
    wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
    nChannels: 2,
    nSamplesPerSec: RATE,
    nAvgBytesPerSec: RATE * 8,
    nBlockAlign: 8,
    wBitsPerSample: 32,
    cbSize: 0,
};

/// An event handle, owned.
struct Event(HANDLE);

// A kernel event may be signalled and waited on from any thread.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    fn new() -> Result<Self> {
        Ok(Self(unsafe { CreateEventW(None, false, false, None)? }))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Recording on a thread of its own.
pub(super) struct Recording {
    stop: Arc<Event>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Recording {
    /// Start recording, or say in words why this computer can't.
    pub(super) fn start(parts: Parts) -> Result<Self, String> {
        let failed = |error: anyhow::Error| format!("Can't record its sound: {error:#}");
        let stop = Arc::new(Event::new().map_err(failed)?);
        let (ready, started) = mpsc::sync_channel(1);
        let thread = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("seam-sound-record".into())
                .spawn(move || run(&parts, &stop, ready))
                .map_err(|error| failed(error.into()))?
        };
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(reason)) => {
                let _ = thread.join();
                Err(reason)
            }
            Err(_) => {
                let _ = unsafe { SetEvent(stop.0) };
                Err("Recording its sound didn't start".into())
            }
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let _ = unsafe { SetEvent(self.stop.0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(parts: &Parts, stop: &Event, ready: mpsc::SyncSender<Result<(), String>>) {
    wasapi::enter_com();
    let _priority = audio_thread_priority::promote_current_thread_to_real_time(480, RATE).ok();
    let started = (|| -> Result<_, String> {
        let mut devices =
            Devices::new().map_err(|error| format!("Can't record its sound: {error:#}"))?;
        let mut mixer = Mixer::default();
        devices.look(Instant::now());
        let failure = mixer.update(&devices, Instant::now());
        if mixer.recorded.is_empty() {
            return Err(failure.map_or_else(
                || "It has no speakers or headphones to share the sound of".into(),
                |error| format!("Can't record its sound: {error:#}"),
            ));
        }
        Ok((devices, mixer))
    })();
    let (mut devices, mut mixer) = match started {
        Ok(started) => {
            let _ = ready.send(Ok(()));
            started
        }
        Err(reason) => {
            let _ = ready.send(Err(reason));
            return;
        }
    };
    if let Err(error) = record(parts, stop, &mut devices, &mut mixer) {
        log::info!("recording stopped: {error:#}");
        parts.failed.store(true, Ordering::Release);
    }
}

/// Mix and hand on what the devices record until stopped.
fn record(parts: &Parts, stop: &Event, devices: &mut Devices, mixer: &mut Mixer) -> Result<()> {
    let mut look = Instant::now() + LOOK;
    let mut handles = Vec::new();
    loop {
        handles.clear();
        handles.push(stop.0);
        handles.extend(
            mixer
                .recorded
                .iter()
                .map(|recorded| recorded.stream.event.0),
        );
        let wait = look.saturating_duration_since(Instant::now());
        let woken = unsafe { WaitForMultipleObjects(&handles, false, wait.as_millis() as u32) };
        let index = woken.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
        if woken != WAIT_TIMEOUT {
            match index {
                0 => return Ok(()),
                index if index < handles.len() => mixer.arrived(index - 1, parts)?,
                _ => anyhow::bail!("waiting for sound failed"),
            }
        }
        let now = Instant::now();
        if now >= look {
            look = now + LOOK;
            devices.look(now);
            if let Some(error) = mixer.update(devices, now) {
                log::debug!("can't record a device: {error:#}");
            }
            if mixer.recorded.is_empty() {
                anyhow::bail!("no device can be recorded");
            }
        }
    }
}

/// One device being recorded.
struct Recorded {
    id: String,
    name: String,
    stream: Stream,
    follower: Follower,
    arrived: Instant,
}

/// The devices being recorded; the first leads.
#[derive(Default)]
struct Mixer {
    recorded: Vec<Recorded>,
    /// Devices that couldn't be recorded, left alone until devices change.
    failed: Vec<String>,
    /// The devices' listing these were last updated from.
    listing: u64,
}

impl Mixer {
    /// Sound arrived from the device at `index`.
    fn arrived(&mut self, index: usize, parts: &Parts) -> Result<()> {
        let now = Instant::now();
        if index > 0 && now - self.recorded[0].arrived > STALL {
            // The leader stopped: this device leads now, and the leader follows.
            self.recorded.swap(0, index);
            self.recorded[index].follower.restart();
            log::info!("{} leads the recording", self.recorded[0].name);
            return self.arrived(0, parts);
        }
        let recorded = &mut self.recorded[index];
        recorded.arrived = now;
        if index > 0 {
            if let Err(error) = recorded.stream.deliver(recorded.follower.waiting()) {
                self.lose(index, &error);
            }
            return Ok(());
        }
        let mut out = parts.spares.try_recv().unwrap_or_default();
        out.clear();
        // What waited while this device followed, then what just arrived.
        recorded.follower.take(&mut out);
        if let Err(error) = recorded.stream.deliver(&mut out) {
            self.lose(0, &error);
            return Ok(());
        }
        for recorded in &mut self.recorded[1..] {
            if recorded.follower.mix_into(&mut out)? {
                log::info!("sound from {} ran short", recorded.name);
            }
        }
        let quiet = parts.quiet.load(Ordering::Relaxed) && dsp::silent(&out);
        if !out.is_empty() && !quiet {
            // A full queue means the network is behind: this one is dropped.
            let _ = parts.sender.try_send(out);
        }
        Ok(())
    }

    /// Stop recording a device that failed, such as one unplugged.
    fn lose(&mut self, index: usize, error: &anyhow::Error) {
        let recorded = self.recorded.remove(index);
        log::info!("stopped recording {}: {error:#}", recorded.name);
        self.failed.push(recorded.id);
    }

    /// Record every device that played lately, and stop recording the rest.
    /// Returns why a device couldn't be recorded, if one couldn't.
    fn update(&mut self, devices: &Devices, now: Instant) -> Option<anyhow::Error> {
        if devices.listing != self.listing {
            self.listing = devices.listing;
            self.failed.clear();
        }
        let wanted = devices.wanted(now);
        self.recorded.retain(|recorded| {
            let keep = wanted.contains(&recorded.id.as_str());
            if !keep {
                log::info!("no longer recording {}", recorded.name);
            }
            keep
        });
        let mut failure = None;
        for id in wanted {
            let known = self.recorded.iter().any(|recorded| recorded.id == id);
            if known || self.failed.iter().any(|failed| failed == id) || self.recorded.len() >= MOST
            {
                continue;
            }
            let name = devices.name(id);
            let opened = devices
                .device(id)
                .context("the device is gone")
                .and_then(Stream::open)
                .and_then(|stream| {
                    Ok(Recorded {
                        id: id.to_owned(),
                        name: name.clone(),
                        stream,
                        follower: Follower::new(RATE)?,
                        arrived: now,
                    })
                });
            match opened {
                Ok(recorded) => {
                    log::info!("recording the sound of {name} ({RATE} Hz)");
                    self.recorded.push(recorded);
                }
                Err(error) => {
                    log::info!("can't record {name}: {error:#}");
                    self.failed.push(id.to_owned());
                    failure = Some(error);
                }
            }
        }
        failure
    }
}

/// A running recording of one device.
struct Stream {
    // Dropped in this order: the clients before the event they signal.
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
}

impl Stream {
    fn open(device: &IMMDevice) -> Result<Self> {
        let event = Event::new()?;
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                ROOM,
                0,
                &FORMAT,
                None,
            )?;
            client.SetEventHandle(event.0)?;
            let capture = client.GetService()?;
            client.Start()?;
            Ok(Self {
                client,
                capture,
                event,
            })
        }
    }

    /// Append what has arrived, as interleaved stereo.
    fn deliver(&self, into: &mut Vec<f32>) -> Result<()> {
        while unsafe { self.capture.GetNextPacketSize()? } > 0 {
            let mut data = std::ptr::null_mut();
            let (mut frames, mut flags) = (0u32, 0u32);
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?
            };
            let samples = frames as usize * 2;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                into.resize(into.len() + samples, 0.0);
            } else {
                into.extend_from_slice(unsafe { std::slice::from_raw_parts(data.cast(), samples) });
            }
            unsafe { self.capture.ReleaseBuffer(frames)? };
        }
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

/// One output device, and when it last played.
struct Speakers {
    id: String,
    device: IMMDevice,
    meter: Option<IAudioMeterInformation>,
    heard: Option<Instant>,
}

/// Every output device, kept current as devices come and go.
struct Devices {
    enumerator: IMMDeviceEnumerator,
    changed: Arc<AtomicBool>,
    changes: IMMNotificationClient,
    list: Vec<Speakers>,
    default: Option<String>,
    /// Counts the times the devices were listed.
    listing: u64,
}

impl Devices {
    fn new() -> Result<Self> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let changed = Arc::new(AtomicBool::new(false));
        let changes: IMMNotificationClient = Changes(changed.clone()).into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&changes)? };
        let mut devices = Self {
            enumerator,
            changed,
            changes,
            list: Vec::new(),
            default: None,
            listing: 0,
        };
        devices.refresh()?;
        Ok(devices)
    }

    /// List the devices again, keeping what is known about each.
    fn refresh(&mut self) -> Result<()> {
        self.changed.store(false, Ordering::Relaxed);
        self.listing += 1;
        unsafe {
            self.default = self
                .enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .ok()
                .and_then(|device| wasapi::id_of(&device).ok());
            let found = self
                .enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut list = Vec::new();
            for index in 0..found.GetCount()? {
                let device = found.Item(index)?;
                let id = wasapi::id_of(&device)?;
                list.push(match self.list.iter().position(|known| known.id == id) {
                    Some(known) => self.list.swap_remove(known),
                    None => Speakers {
                        meter: device.Activate(CLSCTX_ALL, None).ok(),
                        id,
                        device,
                        heard: None,
                    },
                });
            }
            self.list = list;
        }
        Ok(())
    }

    /// Read every level meter.
    fn look(&mut self, now: Instant) {
        if self.changed.load(Ordering::Relaxed)
            && let Err(error) = self.refresh()
        {
            log::debug!("can't list the speakers: {error:#}");
        }
        for speakers in &mut self.list {
            let peak = speakers
                .meter
                .as_ref()
                .and_then(|meter| unsafe { meter.GetPeakValue() }.ok())
                .unwrap_or(0.0);
            if peak >= dsp::SILENCE {
                speakers.heard = Some(now);
            }
        }
    }

    /// The devices to record: the default speakers, where most sound plays,
    /// and each other device that played within `LINGER`.
    fn wanted(&self, now: Instant) -> Vec<&str> {
        let default = self
            .list
            .iter()
            .find(|speakers| Some(&speakers.id) == self.default.as_ref())
            .or(self.list.first());
        let mut wanted: Vec<&str> = default
            .iter()
            .map(|speakers| speakers.id.as_str())
            .collect();
        for speakers in &self.list {
            let lately = speakers.heard.is_some_and(|at| now - at <= LINGER);
            if lately && !wanted.contains(&speakers.id.as_str()) {
                wanted.push(&speakers.id);
            }
        }
        wanted
    }

    fn device(&self, id: &str) -> Option<&IMMDevice> {
        self.list
            .iter()
            .find(|speakers| speakers.id == id)
            .map(|speakers| &speakers.device)
    }

    /// A device's name, for the log.
    fn name(&self, id: &str) -> String {
        self.device(id)
            .and_then(|device| unsafe {
                let store = device.OpenPropertyStore(STGM_READ).ok()?;
                Some(store.GetValue(&PKEY_Device_FriendlyName).ok()?.to_string())
            })
            .unwrap_or_else(|| "the speakers".into())
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.changes)
        };
    }
}

/// Notes that the devices, or the default speakers, changed.
#[implement(IMMNotificationClient)]
struct Changes(Arc<AtomicBool>);

impl Changes {
    fn note(&self) -> windows_core::Result<()> {
        self.0.store(true, Ordering::Relaxed);
        Ok(())
    }
}

impl IMMNotificationClient_Impl for Changes_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows_core::Result<()> {
        self.note()
    }

    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows_core::Result<()> {
        self.note()
    }

    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows_core::Result<()> {
        self.note()
    }

    fn OnDefaultDeviceChanged(
        &self,
        _: EDataFlow,
        _: ERole,
        _: &PCWSTR,
    ) -> windows_core::Result<()> {
        self.note()
    }

    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows_core::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "records this computer's sound for a few seconds"]
    fn starts_and_stops_cleanly() {
        for _ in 0..3 {
            let (sender, packets) = crossbeam_channel::bounded(16);
            let (spare, spares) = crossbeam_channel::bounded(16);
            let parts = Parts {
                sender,
                spares,
                failed: Default::default(),
                quiet: Default::default(),
            };
            let recording = Recording::start(parts).unwrap();
            let started = Instant::now();
            let mut received = 0;
            while started.elapsed() < Duration::from_secs(2) {
                if let Ok(buffer) = packets.recv_timeout(Duration::from_millis(100)) {
                    received += 1;
                    let _ = spare.try_send(buffer);
                }
                // Churn the heap, so any corruption is found here.
                let churn: Vec<Vec<u8>> = (0..64).map(|size| vec![1; size * 97]).collect();
                drop(churn);
            }
            drop(recording);
            eprintln!("received {received} buffers");
        }
    }
}
