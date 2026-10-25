//! Microphone capture and speaker playback for real-time voice.

use crate::REALTIME_SAMPLE_RATE;
use crate::wake::cancelled_by;
use anyhow::{Result, anyhow};
use futures::channel::mpsc;
use rodio::Source;
use rodio::cpal::Sample as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// 80 ms at 24 kHz mono.
const REALTIME_FRAME_SAMPLES: usize = 1_920;

/// One realtime frame's worth of the echo canceller's 48 kHz stereo.
const INPUT_SAMPLES_PER_FRAME: usize = REALTIME_FRAME_SAMPLES * 2 * 2;

const PLAYBACK_SAMPLE_RATE: rodio::SampleRate = match rodio::SampleRate::new(REALTIME_SAMPLE_RATE) {
    Some(rate) => rate,
    None => panic!("the realtime sample rate is not zero"),
};

/// 20 ms of played audio per output level update.
const OUTPUT_METER_SAMPLES: usize = 480;

/// Downmixes echo-cancelled 48 kHz stereo to mono and halves the rate.
///
/// 48 kHz to 24 kHz is exactly 2:1, so averaging each pair of stereo frames
/// is the whole resampler.
pub fn to_realtime_frame(cancelled_48k_stereo: &[f32]) -> Vec<i16> {
    cancelled_48k_stereo
        .chunks_exact(4)
        .map(|chunk| (chunk.iter().sum::<f32>() / 4.0).to_sample::<i16>())
        .collect()
}

/// Root-mean-square level, 0 for silence and 1 for full scale.
pub fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_of_squares: f32 = samples
        .iter()
        .map(|sample| (f32::from(*sample) / f32::from(i16::MAX)).powi(2))
        .sum();
    (sum_of_squares / samples.len() as f32).sqrt().min(1.0)
}

/// Microphone and speaker loudness, written by the audio threads and read by
/// whatever draws them.
#[derive(Default)]
pub struct VoiceLevels {
    input: AtomicU32,
    output: AtomicU32,
}

impl VoiceLevels {
    pub fn set_input(&self, level: f32) {
        self.input.store(level.to_bits(), Ordering::Relaxed);
    }

    pub fn set_output(&self, level: f32) {
        self.output.store(level.to_bits(), Ordering::Relaxed);
    }

    pub fn input(&self) -> f32 {
        f32::from_bits(self.input.load(Ordering::Relaxed))
    }

    pub fn output(&self) -> f32 {
        f32::from_bits(self.output.load(Ordering::Relaxed))
    }
}

/// Echo-cancelled realtime frames from `source`.
///
/// The canceller must see the 48 kHz stereo it matches against the speaker;
/// see [`crate::wake`] for why downmixing first fails silently.
fn cancel_then_convert(
    source: impl Source,
    cancel: impl FnMut(&mut [i16]) -> Result<()>,
) -> impl Iterator<Item = Vec<i16>> {
    let mut cancelled = cancelled_by(source, cancel);
    std::iter::from_fn(move || {
        let input: Vec<f32> = cancelled.by_ref().take(INPUT_SAMPLES_PER_FRAME).collect();
        (input.len() == INPUT_SAMPLES_PER_FRAME).then(|| to_realtime_frame(&input))
    })
}

/// Capture runs while this is alive.
pub struct Microphone {
    stop: Arc<AtomicBool>,
}

impl Drop for Microphone {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Microphone {
    /// A microphone that captures nothing, for tests that feed frames by hand.
    pub fn for_test() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Opens the microphone and streams echo-cancelled 24 kHz mono frames of
/// [`REALTIME_FRAME_SAMPLES`].
pub fn start_microphone(
    mut echo_canceller: audio::EchoCanceller,
    levels: Arc<VoiceLevels>,
) -> Result<(Microphone, mpsc::UnboundedReceiver<Vec<i16>>)> {
    let stop = Arc::new(AtomicBool::new(false));
    let (frame_sender, frame_receiver) = mpsc::unbounded();
    let (opened_sender, opened_receiver) = std::sync::mpsc::sync_channel(1);

    // The microphone iterator blocks, so it gets a thread of its own, and the
    // stream is opened there because it is pulled from there.
    std::thread::Builder::new()
        .name("realtime-microphone".into())
        .spawn({
            let stop = stop.clone();
            move || {
                let microphone = match audio::open_input_stream(None) {
                    Ok(microphone) => {
                        opened_sender.send(Ok(())).ok();
                        microphone
                    }
                    Err(error) => {
                        opened_sender
                            .send(Err(anyhow!("opening the microphone: {error}")))
                            .ok();
                        return;
                    }
                };
                let frames = cancel_then_convert(microphone, move |buffer| {
                    echo_canceller.process_stream(buffer)
                });
                for frame in frames {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    levels.set_input(rms(&frame));
                    if frame_sender.unbounded_send(frame).is_err() {
                        break;
                    }
                }
                levels.set_input(0.0);
            }
        })?;

    opened_receiver
        .recv()
        .map_err(|_| anyhow!("the microphone thread exited before opening the microphone"))??;
    Ok((Microphone { stop }, frame_receiver))
}

/// What the server should truncate its record of the interrupted item to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayedAudio {
    pub item_id: String,
    pub content_index: u32,
    pub audio_end_ms: u32,
}

struct PlayingItem {
    item_id: String,
    content_index: u32,
    played_samples: Arc<AtomicU64>,
}

/// The assistant's voice, played through the shared output mixer.
pub struct VoicePlayback {
    mixer: rodio::mixer::Mixer,
    player: rodio::Player,
    levels: Arc<VoiceLevels>,
    // ponytail: only the newest item is tracked; if a new item is appended
    // while an older one is still audible, stopping reports the newer one.
    current: Option<PlayingItem>,
}

impl VoicePlayback {
    pub fn new(mixer: rodio::mixer::Mixer, levels: Arc<VoiceLevels>) -> Self {
        let player = rodio::Player::connect_new(&mixer);
        Self {
            mixer,
            player,
            levels,
            current: None,
        }
    }

    pub fn append(&mut self, item_id: &str, content_index: u32, samples: Vec<i16>) {
        let is_current = self.current.as_ref().is_some_and(|current| {
            current.item_id == item_id && current.content_index == content_index
        });
        if !is_current {
            self.current = Some(PlayingItem {
                item_id: item_id.to_string(),
                content_index,
                played_samples: Arc::new(AtomicU64::new(0)),
            });
        }
        if let Some(current) = &self.current {
            self.player.append(MeteredSource::new(
                samples,
                current.played_samples.clone(),
                self.levels.clone(),
            ));
        }
    }

    /// Silences playback and reports how much of the current item was heard,
    /// or `None` if everything appended had already finished playing.
    pub fn stop(&mut self) -> Option<PlayedAudio> {
        let was_playing = self.is_playing();
        // Dropping the player is the only stop that neither races the audio
        // thread nor blocks this one; see `read_aloud`'s `RodioSink`.
        self.player = rodio::Player::connect_new(&self.mixer);
        self.levels.set_output(0.0);
        let current = self.current.take()?;
        if !was_playing {
            return None;
        }
        let played_samples = current.played_samples.load(Ordering::Relaxed);
        let audio_end_ms = played_samples * 1_000 / u64::from(REALTIME_SAMPLE_RATE);
        Some(PlayedAudio {
            item_id: current.item_id,
            content_index: current.content_index,
            audio_end_ms: u32::try_from(audio_end_ms).unwrap_or(u32::MAX),
        })
    }

    pub fn is_playing(&self) -> bool {
        !self.player.empty()
    }

    /// How much of the current item the audio thread has pulled, for
    /// noticing an output stream that has stopped sounding.
    pub fn played_samples(&self) -> u64 {
        self.current
            .as_ref()
            .map_or(0, |current| current.played_samples.load(Ordering::Relaxed))
    }
}

#[cfg(any(test, feature = "test-support"))]
impl VoicePlayback {
    /// Playback on a mixer nothing pulls from, so nothing is ever heard.
    pub fn for_test(levels: Arc<VoiceLevels>) -> Self {
        let (mixer, _source) = rodio::mixer::mixer(audio::CHANNEL_COUNT, audio::SAMPLE_RATE);
        Self::new(mixer, levels)
    }

    /// Pretends the audio thread has played `samples` of the current item.
    pub fn set_played_samples_for_test(&self, samples: u64) {
        if let Some(current) = &self.current {
            current.played_samples.store(samples, Ordering::Relaxed);
        }
    }
}

/// 24 kHz mono PCM that counts and meters samples as the audio thread pulls
/// them, so `audio_end_ms` reflects what was heard rather than received.
struct MeteredSource {
    samples: std::vec::IntoIter<i16>,
    played_samples: Arc<AtomicU64>,
    levels: Arc<VoiceLevels>,
    meter_window: Vec<i16>,
    finished: bool,
}

impl MeteredSource {
    fn new(samples: Vec<i16>, played_samples: Arc<AtomicU64>, levels: Arc<VoiceLevels>) -> Self {
        Self {
            samples: samples.into_iter(),
            played_samples,
            levels,
            meter_window: Vec::with_capacity(OUTPUT_METER_SAMPLES),
            finished: false,
        }
    }
}

impl Iterator for MeteredSource {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(sample) = self.samples.next() else {
            if !self.finished {
                self.finished = true;
                self.levels.set_output(0.0);
            }
            return None;
        };
        self.played_samples.fetch_add(1, Ordering::Relaxed);
        self.meter_window.push(sample);
        if self.meter_window.len() == OUTPUT_METER_SAMPLES {
            self.levels.set_output(rms(&self.meter_window));
            self.meter_window.clear();
        }
        Some(sample.to_sample())
    }
}

// A source dropped mid-stream (the player was replaced on barge-in) never
// reaches its end, so without this the glow would keep showing its last level.
impl Drop for MeteredSource {
    fn drop(&mut self) {
        if !self.finished {
            self.levels.set_output(0.0);
        }
    }
}

impl Source for MeteredSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.samples.len())
    }

    fn channels(&self) -> rodio::ChannelCount {
        rodio::nz!(1)
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        PLAYBACK_SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn mic_chain_cancels_before_downmixing() {
        // Left and right are opposites, so a downmix before cancellation would
        // hand the canceller silence.
        let stereo: Vec<f32> = (0..INPUT_SAMPLES_PER_FRAME)
            .map(|index| if index % 2 == 0 { 0.5 } else { -0.5 })
            .collect();
        let source =
            rodio::buffer::SamplesBuffer::new(audio::CHANNEL_COUNT, audio::SAMPLE_RATE, stereo);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut frames = cancel_then_convert(source, {
            let seen = seen.clone();
            move |buffer: &mut [i16]| {
                if let Ok(mut seen) = seen.lock() {
                    seen.push(buffer.to_vec());
                }
                Ok(())
            }
        });

        let frame = frames.next().expect("one full frame");
        assert_eq!(frame.len(), REALTIME_FRAME_SAMPLES);

        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), INPUT_SAMPLES_PER_FRAME / audio::BUFFER_SIZE);
        let first = &seen[0];
        // 10 ms of 2-channel 48 kHz audio.
        assert_eq!(
            first.len(),
            audio::SAMPLE_RATE.get() as usize / 100 * audio::CHANNEL_COUNT.get() as usize
        );
        assert!(
            first[0] > 0 && first[1] < 0,
            "canceller saw {:?}",
            &first[..2]
        );
    }

    #[test]
    fn to_realtime_frame_halves_the_sample_count() {
        let input = vec![0.25_f32; 1920];
        assert_eq!(to_realtime_frame(&input).len(), input.len() / 2 / 2);
    }

    fn test_playback() -> VoicePlayback {
        let (mixer, _source) = rodio::mixer::mixer(audio::CHANNEL_COUNT, audio::SAMPLE_RATE);
        VoicePlayback::new(mixer, Arc::new(VoiceLevels::default()))
    }

    #[test]
    fn audio_end_ms_counts_what_was_played_not_received() {
        let mut playback = test_playback();
        playback.append("item_1", 0, vec![1_000; 24_000]);
        playback
            .current
            .as_ref()
            .expect("an item is playing")
            .played_samples
            .store(9_600, Ordering::Relaxed);

        let played = playback.stop().expect("something was playing");
        assert_eq!(played.item_id, "item_1");
        assert_eq!(played.content_index, 0);
        assert_eq!(played.audio_end_ms, 400);
    }

    #[test]
    fn stop_replaces_the_player() {
        let mut playback = test_playback();
        playback.append("item_1", 0, vec![1_000; 24_000]);
        assert!(playback.is_playing());
        playback.stop();
        assert!(!playback.is_playing());
        playback.append("item_2", 0, vec![1_000; 24_000]);
        assert!(playback.is_playing());
    }

    #[test]
    fn rms_of_silence_is_zero_and_full_scale_is_one() {
        assert_eq!(rms(&[0; 480]), 0.0);
        assert_eq!(rms(&[i16::MAX; 480]), 1.0);
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn played_samples_are_counted_and_metered_as_the_audio_thread_pulls_them() {
        let levels = Arc::new(VoiceLevels::default());
        let played_samples = Arc::new(AtomicU64::new(0));
        let source =
            MeteredSource::new(vec![i16::MAX; 960], played_samples.clone(), levels.clone());
        assert_eq!(played_samples.load(Ordering::Relaxed), 0);
        assert_eq!(source.count(), 960);
        assert_eq!(played_samples.load(Ordering::Relaxed), 960);
        assert_eq!(levels.output(), 0.0, "exhausted playback reads as silence");
    }

    #[test]
    fn a_source_dropped_mid_stream_reads_as_silence() {
        let levels = Arc::new(VoiceLevels::default());
        let mut source = MeteredSource::new(
            vec![i16::MAX; 960],
            Arc::new(AtomicU64::new(0)),
            levels.clone(),
        );
        for _ in 0..OUTPUT_METER_SAMPLES {
            source.next();
        }
        assert!(levels.output() > 0.0);
        drop(source);
        assert_eq!(levels.output(), 0.0);
    }
}
