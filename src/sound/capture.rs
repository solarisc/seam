//! Records what this computer plays, for the computer that controls it.
//!
//! Windows records each device that plays and mixes them (see `loopback`).
//! macOS 14.6 and later record the default speakers' mix through a Core Audio
//! process tap, after the person allows Seam to record the computer's sound.
#[cfg(not(windows))]
use super::dsp;
#[cfg(not(windows))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Capture buffers queued for the network, about 10 ms each. When the network
/// falls further behind, newer buffers are dropped until it catches up.
const QUEUE: usize = 16;

/// Recording delivered as interleaved stereo.
pub(super) struct Capture {
    source: Source,
    pub(super) rate: u32,
    pub(super) packets: Receiver<Vec<f32>>,
    spare: Sender<Vec<f32>>,
    failed: Arc<AtomicBool>,
    /// Set while nothing plays: silent buffers are then dropped as recorded,
    /// so a device that plays silence all along wakes nothing else.
    quiet: Arc<AtomicBool>,
}

#[cfg(windows)]
type Source = super::loopback::Recording;

/// The default speakers' mix.
#[cfg(not(windows))]
struct Source {
    _stream: cpal::Stream,
    device: cpal::Device,
}

/// What a recording hands its buffers through.
pub(super) struct Parts {
    pub(super) sender: Sender<Vec<f32>>,
    pub(super) spares: Receiver<Vec<f32>>,
    pub(super) failed: Arc<AtomicBool>,
    pub(super) quiet: Arc<AtomicBool>,
}

impl Capture {
    /// Start recording, or say in words why this computer can't.
    pub(super) fn start(host: &cpal::Host) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        if !mac_version().is_some_and(|version| version >= (14, 6)) {
            return Err("Sharing a Mac's sound needs macOS 14.6 or later".into());
        }
        let (sender, packets) = crossbeam_channel::bounded(QUEUE);
        let (spare, spares) = crossbeam_channel::bounded(QUEUE);
        let failed = Arc::new(AtomicBool::new(false));
        let quiet = Arc::new(AtomicBool::new(false));
        let parts = Parts {
            sender,
            spares,
            failed: failed.clone(),
            quiet: quiet.clone(),
        };
        let (source, rate) = open(host, parts)?;
        Ok(Self {
            source,
            rate,
            packets,
            spare,
            failed,
            quiet,
        })
    }

    /// Nothing is playing and the other computer knows: until sound returns,
    /// silent buffers needn't reach the network thread.
    pub(super) fn set_quiet(&self, quiet: bool) {
        self.quiet.store(quiet, Ordering::Relaxed);
    }

    /// Hand a sent buffer back for the recording to fill again.
    pub(super) fn recycle(&self, buffer: Vec<f32>) {
        let _ = self.spare.try_send(buffer);
    }

    /// Still recording. After a failure, or on macOS when the person picks
    /// other speakers, the recording starts again.
    pub(super) fn current(&self, host: &cpal::Host) -> bool {
        if self.failed.load(Ordering::Acquire) {
            return false;
        }
        // Windows follows the devices by itself.
        #[cfg(windows)]
        let _ = (host, &self.source);
        #[cfg(not(windows))]
        if !host
            .default_output_device()
            .is_some_and(|default| default == self.source.device)
        {
            return false;
        }
        true
    }
}

#[cfg(windows)]
fn open(_: &cpal::Host, parts: Parts) -> Result<(Source, u32), String> {
    let recording = super::loopback::Recording::start(parts)?;
    Ok((recording, super::loopback::RATE))
}

#[cfg(not(windows))]
fn open(host: &cpal::Host, parts: Parts) -> Result<(Source, u32), String> {
    let device = host
        .default_output_device()
        .ok_or("It has no speakers or headphones to share the sound of")?;
    let supported = device
        .default_output_config()
        .map_err(|error| format!("Can't read its sound format: {error}"))?;
    let config = supported.config();
    let channels = usize::from(config.channels);
    use cpal::SampleFormat as Format;
    let stream = match supported.sample_format() {
        Format::F32 => record::<f32>(&device, config, channels, parts),
        Format::I16 => record::<i16>(&device, config, channels, parts),
        Format::I32 => record::<i32>(&device, config, channels, parts),
        Format::F64 => record::<f64>(&device, config, channels, parts),
        Format::U16 => record::<u16>(&device, config, channels, parts),
        Format::U8 => record::<u8>(&device, config, channels, parts),
        Format::I8 => record::<i8>(&device, config, channels, parts),
        other => return Err(format!("Its sound format ({other}) isn't supported")),
    }
    .map_err(|error| format!("Can't record its sound: {error}"))?;
    stream
        .play()
        .map_err(|error| format!("Can't record its sound: {error}"))?;
    log::info!(
        "recording the sound of {device} at {} Hz, {channels} channel(s)",
        config.sample_rate
    );
    let rate = config.sample_rate;
    Ok((
        Source {
            _stream: stream,
            device,
        },
        rate,
    ))
}

#[cfg(not(windows))]
fn record<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    parts: Parts,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let Parts {
        sender,
        spares,
        failed,
        quiet,
    } = parts;
    // Building on an output device records what it plays (loopback).
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            if quiet.load(Ordering::Relaxed) && dsp::silent_as_recorded(data) {
                return;
            }
            let mut buffer = spares.try_recv().unwrap_or_default();
            buffer.clear();
            dsp::downmix(data, channels, &mut buffer);
            // A full queue means the network is behind: this buffer is dropped.
            let _ = sender.try_send(buffer);
        },
        move |error: cpal::Error| {
            if error.kind() != cpal::ErrorKind::Xrun {
                failed.store(true, Ordering::Release);
            }
        },
        None,
    )
}

/// The running macOS version, such as `(15, 1)`.
#[cfg(target_os = "macos")]
fn mac_version() -> Option<(u32, u32)> {
    let mut buffer = [0u8; 32];
    let mut size = buffer.len();
    let found = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    found.then(|| parse_version(&buffer[..size]))?
}

#[cfg(any(target_os = "macos", test))]
fn parse_version(text: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(text).ok()?.trim_end_matches('\0');
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |minor| minor.parse().ok())?;
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_versions_parse_with_or_without_a_minor_number() {
        assert_eq!(parse_version(b"14.6.1\0"), Some((14, 6)));
        assert_eq!(parse_version(b"26.0"), Some((26, 0)));
        assert_eq!(parse_version(b"15"), Some((15, 0)));
        assert_eq!(parse_version(b"beta"), None);
        assert!(parse_version(b"14.5").unwrap() < (14, 6));
    }
}
