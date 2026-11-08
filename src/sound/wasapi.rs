//! Windows: the speakers' own buffer, written to directly, and the player
//! that pauses, closes and follows them.
//!
//! Sound goes straight into the speakers' shared-mode buffer as each packet
//! arrives (see `push`), so it waits in no other buffer, and no thread wakes
//! on a timer to feed the speakers.
use super::{
    pacing::{CLOSE_AFTER, DEVICE_CHECK, PAUSE_AFTER, UNTIL_WOKEN},
    push::{Push, Speaker},
};
use anyhow::{Context, Result, bail};
use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};
use windows::Win32::{
    Media::{
        Audio::{
            AUDCLNT_SHAREMODE_SHARED, IAudioClient, IAudioRenderClient, IMMDevice,
            IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole,
            eRender,
        },
        KernelStreaming::WAVE_FORMAT_EXTENSIBLE,
        Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
    },
    System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    },
};

/// The most the speakers' buffer holds, in 100 ns units: room for a slow
/// network's target. Only what the target needs is ever queued in it.
const CAPACITY: i64 = 2_000_000;

/// Join this thread to the multithreaded apartment, once.
pub(super) fn enter_com() {
    thread_local! {
        static JOINED: () = {
            // Already joined, or joined as single-threaded by someone else,
            // both still let these calls work.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        };
    }
    JOINED.with(|()| {});
}

fn default_speakers() -> Result<IMMDevice> {
    enter_com();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("no speakers or headphones are connected")
    }
}

pub(super) fn id_of(device: &IMMDevice) -> Result<String> {
    unsafe {
        let id = device.GetId()?;
        let text = id.to_string();
        CoTaskMemFree(Some(id.0 as *const _));
        Ok(text?)
    }
}

/// The default speakers' shared-mode buffer.
pub(super) struct Wasapi {
    client: IAudioClient,
    render: IAudioRenderClient,
    capacity: usize,
    channels: usize,
    rate: u32,
    running: bool,
    id: String,
}

// The interfaces belong to the multithreaded apartment, which every thread
// using them joins first; a mutex keeps them to one thread at a time.
unsafe impl Send for Wasapi {}

impl Wasapi {
    pub(super) fn open() -> Result<Self> {
        let device = default_speakers()?;
        let id = id_of(&device)?;
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let opened = (|| {
                let mix: &WAVEFORMATEX = &*format;
                let float = mix.wBitsPerSample == 32
                    && match u32::from(mix.wFormatTag) {
                        WAVE_FORMAT_IEEE_FLOAT => true,
                        WAVE_FORMAT_EXTENSIBLE => {
                            // Packed: copy the field out rather than borrow it.
                            let extensible = format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned();
                            let subformat = extensible.SubFormat;
                            subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
                        }
                        _ => false,
                    };
                if !float {
                    bail!("the speakers' format isn't supported");
                }
                // No event: sound is written as it arrives, and the buffer
                // plays on by itself, silence once it runs out.
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, CAPACITY, 0, format, None)?;
                Ok((usize::from(mix.nChannels), mix.nSamplesPerSec))
            })();
            CoTaskMemFree(Some(format as *const _));
            let (channels, rate) = opened.context("opening the speakers")?;
            let capacity = client.GetBufferSize()? as usize;
            let render: IAudioRenderClient = client.GetService()?;
            client.Start().context("starting the speakers")?;
            log::info!(
                "sound playing on {} at {rate} Hz, {channels} channel(s), written as it arrives",
                speaker_name()
            );
            Ok(Self {
                client,
                render,
                capacity,
                channels: channels.max(1),
                rate,
                running: true,
                id,
            })
        }
    }

    /// Another device became the default speakers.
    fn replaced(&self) -> bool {
        default_speakers()
            .and_then(|device| id_of(&device))
            .is_ok_and(|id| id != self.id)
    }
}

/// The default speakers' name, for the log.
fn speaker_name() -> String {
    use cpal::traits::HostTrait;
    cpal::default_host()
        .default_output_device()
        .map_or_else(|| "the speakers".into(), |device| device.to_string())
}

impl Speaker for Wasapi {
    fn padding(&mut self) -> Result<usize> {
        Ok(unsafe { self.client.GetCurrentPadding()? } as usize)
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn write(&mut self, samples: &[f32]) -> Result<()> {
        let frames = samples.len() / self.channels;
        if frames == 0 {
            return Ok(());
        }
        let count = u32::try_from(frames)?;
        unsafe {
            let buffer = self.render.GetBuffer(count)?;
            std::ptr::copy_nonoverlapping(
                samples.as_ptr(),
                buffer.cast::<f32>(),
                frames * self.channels,
            );
            self.render.ReleaseBuffer(count, 0)?;
        }
        Ok(())
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn rate(&self) -> u32 {
        self.rate
    }

    fn running(&self) -> bool {
        self.running
    }

    fn start(&mut self) -> Result<()> {
        enter_com();
        unsafe { self.client.Start()? };
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        enter_com();
        unsafe { self.client.Stop()? };
        self.running = false;
        Ok(())
    }
}

fn lock(push: &Mutex<Push<Wasapi>>) -> MutexGuard<'_, Push<Wasapi>> {
    push.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One controlled computer's sound on this computer.
pub(super) struct Player {
    push: Arc<Mutex<Push<Wasapi>>>,
    checked: Instant,
    /// Running speakers had nothing to play since then.
    quiet_since: Option<Instant>,
    /// The speakers paused then.
    paused_at: Option<Instant>,
    /// Why the speakers can't play, for the window.
    pub(super) problem: Option<String>,
    /// Sound arrived since the last look.
    playing: bool,
    received: u64,
    underruns: u64,
    /// The buffer's target last written to the log.
    logged_delay: Duration,
}

impl Player {
    pub(super) fn new() -> Self {
        Self {
            push: Arc::new(Mutex::new(Push::new(48_000, Wasapi::open))),
            checked: Instant::now(),
            quiet_since: None,
            paused_at: None,
            problem: None,
            playing: false,
            received: 0,
            underruns: 0,
            logged_delay: Duration::ZERO,
        }
    }

    /// Where the network thread writes sound as it arrives.
    pub(super) fn buffer(&self) -> Arc<Mutex<Push<Wasapi>>> {
        self.push.clone()
    }

    /// Sound follows at `rate`; the network thread already set it up.
    pub(super) fn format(&mut self, _rate: u32) {}

    /// Sound arrived and started the speakers: look again each second.
    pub(super) fn start(&mut self) {
        self.quiet_since = None;
        self.paused_at = None;
        self.checked = Instant::now();
    }

    /// Sound arrived since the last look.
    pub(super) fn playing(&self) -> bool {
        self.playing
    }

    /// Pause or close idle speakers, follow a change of speakers and report.
    /// Returns how long until this is needed again.
    pub(super) fn tick(&mut self, name: &str) -> Duration {
        let now = Instant::now();
        let mut push = lock(&self.push);
        let received = push.received;
        self.playing = received != self.received;
        self.received = received;
        self.problem.clone_from(&push.problem);
        let (underruns, delay) = (push.pacing().underruns, push.pacing().delay());
        if underruns > self.underruns {
            log::info!(
                "sound from {name} ran short; now buffering {} ms",
                delay.as_millis()
            );
            self.underruns = underruns;
            self.logged_delay = delay;
        } else if delay.abs_diff(self.logged_delay) >= Duration::from_millis(5) {
            log::info!("sound from {name}: buffering {} ms", delay.as_millis());
            self.logged_delay = delay;
        }
        let Some(speaker) = push.speaker() else {
            return UNTIL_WOKEN;
        };
        if speaker.running() {
            if self.playing {
                self.quiet_since = None;
            } else if now - *self.quiet_since.get_or_insert(now) >= PAUSE_AFTER {
                // Paused speakers cost nothing; the next sound resumes them.
                if speaker.stop().is_err() {
                    push.close();
                }
                self.quiet_since = None;
                self.paused_at = Some(now);
                return CLOSE_AFTER;
            }
            if now - self.checked >= DEVICE_CHECK {
                self.checked = now;
                if speaker.replaced() {
                    log::info!("the speakers changed; following them");
                    push.close();
                }
            }
            DEVICE_CHECK
        } else {
            let paused = now - *self.paused_at.get_or_insert(now);
            if paused >= CLOSE_AFTER {
                log::debug!("sound from {name} has been quiet; closing the speakers");
                push.close();
                self.paused_at = None;
                UNTIL_WOKEN
            } else {
                CLOSE_AFTER - paused
            }
        }
    }
}
