use anyhow::{Context as _, Result};
use collections::HashMap;
use cpal::{
    DeviceDescription, DeviceId, default_host,
    traits::{DeviceTrait, HostTrait},
};
use gpui::{App, AsyncApp, BorrowAppContext, Global};

pub(super) use cpal::Sample;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Source, mixer::Mixer, source::Buffered};
use settings::Settings;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use util::ResultExt;

mod echo_canceller;
use echo_canceller::EchoCanceller;
mod rodio_ext;
pub use crate::audio_settings::AudioSettings;
pub use rodio_ext::RodioExt;

use crate::Sound;

use super::{CHANNEL_COUNT, SAMPLE_RATE};
pub const BUFFER_SIZE: usize = // echo canceller and livekit want 10ms of audio
    (SAMPLE_RATE.get() as usize / 100) * CHANNEL_COUNT.get() as usize;

pub fn init(_cx: &mut App) {}

// TODO(jk): this is currently cached only once - we should observe and react instead
pub fn ensure_devices_initialized(cx: &mut App) {
    if cx.has_global::<AvailableAudioDevices>() {
        return;
    }
    cx.default_global::<AvailableAudioDevices>();
    let task = cx
        .background_executor()
        .spawn(async move { get_available_audio_devices() });
    cx.spawn(async move |cx: &mut AsyncApp| {
        let devices = task.await;
        cx.update(|cx| cx.set_global(AvailableAudioDevices(devices)));
        cx.refresh();
    })
    .detach();
}

/// How often the default output device is looked up while playback is running.
/// The lookup is a CoreAudio/WASAPI property read, and a device switch is a
/// human action, so once a second is both cheap and immediate enough.
const DEVICE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// The open output stream, plus what it takes to notice it is no longer the
/// right one to be playing through.
struct Output {
    _handle: MixerDeviceSink,
    mixer: Mixer,
    /// The device this stream is bound to. cpal binds a stream to the device it
    /// was opened on, so when the OS moves the default output — headphones in,
    /// Bluetooth connecting, a display waking up — this stream keeps rendering
    /// to the old device: audible on the wrong speakers at best, and silent
    /// while still consuming samples at worst.
    device_id: Option<DeviceId>,
    /// Raised by cpal's error callback when the stream breaks under us, which
    /// is what a device being unplugged looks like from here.
    failed: Arc<AtomicBool>,
    /// Throttles [`Audio::output_is_stale`], which playback polls.
    last_checked: Option<Instant>,
}

#[derive(Default)]
pub struct Audio {
    output: Option<Output>,
    pub echo_canceller: EchoCanceller,
    source_cache: HashMap<Sound, Buffered<Decoder<Cursor<Vec<u8>>>>>,
}

impl Global for Audio {}

impl Audio {
    fn ensure_output_exists(&mut self, output_audio_device: Option<DeviceId>) -> Result<&Mixer> {
        #[cfg(debug_assertions)]
        log::warn!(
            "Audio does not sound correct without optimizations. Use a release build to debug audio issues"
        );

        if self.output.is_none() {
            let failed = Arc::new(AtomicBool::new(false));
            let (output_handle, output_mixer, device_id) = open_output_stream(
                output_audio_device,
                self.echo_canceller.clone(),
                failed.clone(),
            )?;
            self.output = Some(Output {
                _handle: output_handle,
                mixer: output_mixer,
                device_id,
                failed,
                last_checked: None,
            });
        }

        Ok(&self
            .output
            .as_ref()
            .expect("we only get here if opening the outputstream succeeded")
            .mixer)
    }

    /// Whether what the app is playing through has stopped being the right
    /// stream, either because it broke or because the system moved the default
    /// output away from the device it is bound to.
    fn output_is_stale(&mut self, requested_device: Option<&DeviceId>) -> bool {
        let Some(output) = self.output.as_mut() else {
            return false;
        };
        if output.failed.load(Ordering::Relaxed) {
            return true;
        }
        // Only the default follows the system. An explicitly chosen device is
        // the user's decision, and reopening it elsewhere would override that.
        if requested_device.is_some() {
            return false;
        }
        if output
            .last_checked
            .is_some_and(|checked_at| checked_at.elapsed() < DEVICE_CHECK_INTERVAL)
        {
            return false;
        }
        output.last_checked = Some(Instant::now());
        let current = default_host()
            .default_output_device()
            .and_then(|device| device.id().ok());
        // A momentary "no default device" (mid-switch) is not a move to
        // anywhere; reopening then would just fail.
        current.is_some() && current != output.device_id
    }

    /// A player on the current output device, for when the stream the app has
    /// been playing through is no longer the right one. `None` while it still
    /// is, or when no device could be opened.
    ///
    /// `force` reopens even when the device looks unchanged, for callers that
    /// can tell the stream is not sounding: a stream can stop being audible
    /// without cpal reporting anything — an endpoint that went away under a
    /// still-current device id, or a virtual device that swallows what it is
    /// handed. Nothing else ever reopened this stream, which is why such a
    /// stream stayed silent for the life of the process.
    ///
    /// Reopening builds a new mixer, which leaves every player handed out
    /// earlier attached to the old one. Callers therefore have to move their
    /// playback onto the returned player; whatever was queued on the old one is
    /// gone.
    pub fn reconnect_player(cx: &mut App, force: bool) -> Option<rodio::Player> {
        let output_audio_device = AudioSettings::get_global(cx).output_audio_device.clone();
        cx.update_default_global(|this: &mut Self, _cx| {
            if !force && !this.output_is_stale(output_audio_device.as_ref()) {
                return None;
            }
            log::info!("Audio output stream is stale; reopening on the current device");
            this.output.take();
            let output_mixer = this
                .ensure_output_exists(output_audio_device)
                .context("Could not reopen output stream")
                .log_err()?;
            Some(rodio::Player::connect_new(output_mixer))
        })
    }

    pub fn play_sound(sound: Sound, cx: &mut App) {
        let output_audio_device = AudioSettings::get_global(cx).output_audio_device.clone();
        cx.update_default_global(|this: &mut Self, cx| {
            let source = this.sound_source(sound, cx).log_err()?;
            let output_mixer = this
                .ensure_output_exists(output_audio_device)
                .context("Could not get output mixer")
                .log_err()?;

            output_mixer.add(source);
            Some(())
        });
    }

    /// Connects an independent playback queue to the shared output mixer.
    ///
    /// Callers that need to play arbitrary samples use this instead of
    /// `play_sound`, which only handles the bundled `Sound` assets. Returns
    /// `None` when no output device could be opened.
    pub fn connect_player(cx: &mut App) -> Option<rodio::Player> {
        let output_audio_device = AudioSettings::get_global(cx).output_audio_device.clone();
        cx.update_default_global(|this: &mut Self, _cx| {
            let output_mixer = this
                .ensure_output_exists(output_audio_device)
                .context("Could not get output mixer")
                .log_err()?;
            Some(rodio::Player::connect_new(output_mixer))
        })
    }

    pub fn end_call(cx: &mut App) {
        cx.update_default_global(|this: &mut Self, _cx| {
            this.output.take();
        });
    }

    fn sound_source(&mut self, sound: Sound, cx: &App) -> Result<impl Source + use<>> {
        if let Some(wav) = self.source_cache.get(&sound) {
            return Ok(wav.clone());
        }

        let path = format!("sounds/{}.wav", sound.file());
        let bytes = cx
            .asset_source()
            .load(&path)?
            .map(anyhow::Ok)
            .with_context(|| format!("No asset available for path {path}"))??
            .into_owned();
        let cursor = Cursor::new(bytes);
        let source = Decoder::new(cursor)?.buffered();

        self.source_cache.insert(sound, source.clone());

        Ok(source)
    }
}

pub fn open_input_stream(
    device_id: Option<DeviceId>,
) -> anyhow::Result<rodio::microphone::Microphone> {
    let builder = rodio::microphone::MicrophoneBuilder::new();
    let builder = if let Some(id) = device_id {
        // TODO(jk): upstream patch
        // if let Some(input_device) = default_host().device_by_id(id) {
        //     builder.device(input_device);
        // }
        let mut found = None;
        for input in rodio::microphone::available_inputs()? {
            if input.clone().into_inner().id()? == id {
                found = Some(builder.device(input));
                break;
            }
        }
        found.unwrap_or_else(|| builder.default_device())?
    } else {
        builder.default_device()?
    };
    let stream = builder
        .default_config()?
        .prefer_sample_rates([
            SAMPLE_RATE,
            SAMPLE_RATE.saturating_mul(rodio::nz!(2)),
            SAMPLE_RATE.saturating_mul(rodio::nz!(3)),
            SAMPLE_RATE.saturating_mul(rodio::nz!(4)),
        ])
        .prefer_channel_counts([rodio::nz!(1), rodio::nz!(2), rodio::nz!(3), rodio::nz!(4)])
        .prefer_buffer_sizes(512..)
        .open_stream()?;
    log::info!("Opened microphone: {:?}", stream.config());
    Ok(stream)
}

pub fn resolve_device(device_id: Option<&DeviceId>, input: bool) -> anyhow::Result<cpal::Device> {
    if let Some(id) = device_id {
        if let Some(device) = default_host().device_by_id(id) {
            return Ok(device);
        }
        log::warn!("Selected audio device not found, falling back to default");
    }
    if input {
        default_host()
            .default_input_device()
            .context("no audio input device available")
    } else {
        default_host()
            .default_output_device()
            .context("no audio output device available")
    }
}

pub fn open_test_output(device_id: Option<DeviceId>) -> anyhow::Result<MixerDeviceSink> {
    let device = resolve_device(device_id.as_ref(), false)?;
    DeviceSinkBuilder::from_device(device)?
        .open_stream()
        .context("Could not open output stream")
}

pub fn open_output_stream(
    device_id: Option<DeviceId>,
    mut echo_canceller: EchoCanceller,
    failed: Arc<AtomicBool>,
) -> anyhow::Result<(MixerDeviceSink, Mixer, Option<DeviceId>)> {
    let device = resolve_device(device_id.as_ref(), false)?;
    // Recorded from the resolved device rather than from `device_id`, which is
    // `None` for "whatever the system default is" — the case that has to be
    // noticed when the default moves.
    let opened_device_id = device.id().ok();
    let mut output_handle = DeviceSinkBuilder::from_device(device)?
        .with_error_callback(move |error| {
            log::error!("Audio output stream error: {error}");
            failed.store(true, Ordering::Relaxed);
        })
        .open_stream()
        .context("Could not open output stream")?;
    output_handle.log_on_drop(false);
    log::info!("Output stream: {:?}", output_handle);

    let (output_mixer, source) = rodio::mixer::mixer(CHANNEL_COUNT, SAMPLE_RATE);
    // otherwise the mixer ends as it's empty
    output_mixer.add(rodio::source::Zero::new(CHANNEL_COUNT, SAMPLE_RATE));
    let echo_cancelling_source = source // apply echo cancellation just before output
        .inspect_buffer::<BUFFER_SIZE, _>(move |buffer| {
            let mut buf: [i16; _] = buffer.map(|s| s.to_sample());
            echo_canceller.process_reverse_stream(&mut buf)
        });
    output_handle.mixer().add(echo_cancelling_source);

    Ok((output_handle, output_mixer, opened_device_id))
}

#[derive(Clone, Debug)]
pub struct AudioDeviceInfo {
    pub id: DeviceId,
    pub desc: DeviceDescription,
}

impl AudioDeviceInfo {
    pub fn matches_input(&self, is_input: bool) -> bool {
        if is_input {
            self.desc.supports_input()
        } else {
            self.desc.supports_output()
        }
    }

    pub fn matches(&self, id: &DeviceId, is_input: bool) -> bool {
        &self.id == id && self.matches_input(is_input)
    }
}

impl std::fmt::Display for AudioDeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.desc.name(), self.id)
    }
}

fn get_available_audio_devices() -> Vec<AudioDeviceInfo> {
    let Some(devices) = default_host().devices().ok() else {
        return Vec::new();
    };
    devices
        .filter_map(|device| {
            let id = device.id().ok()?;
            let desc = device.description().ok()?;
            Some(AudioDeviceInfo { id, desc })
        })
        .collect()
}

#[derive(Default, Clone, Debug)]
pub struct AvailableAudioDevices(pub Vec<AudioDeviceInfo>);

impl Global for AvailableAudioDevices {}
