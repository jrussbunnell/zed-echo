use crate::provider::Pcm;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Playback seam. `RodioSink` is the real one; `FakeSink` keeps the player's
/// tests free of any dependency on a sound card.
///
/// The sink speaks in *utterances* even though streaming synthesis hands it an
/// utterance as several chunks. Keeping that grouping here rather than in the
/// player is deliberate: the player derives the speaking index and the karaoke
/// highlight from `queued()` and `position()`, and both of those stay correct
/// only while one queue slot means one utterance.
pub trait AudioSink: 'static {
    /// Appends one chunk of an utterance. Consecutive appends carrying the same
    /// `utterance` token belong to the same utterance and play back-to-back as
    /// one unit for queueing and position purposes.
    fn append_chunk(&self, utterance: u64, pcm: Pcm);
    fn clear(&self);
    /// Number of utterances still queued, including the one playing.
    fn queued(&self) -> usize;
    fn set_speed(&self, speed: f32);
    /// Scales playback loudness without touching the queue or the position.
    ///
    /// Separate from `pause` because ducking under a speaker is not the same
    /// decision as stopping for one: a listener who cleared their throat
    /// should not lose the sentence.
    fn set_volume(&self, volume: f32);
    fn stop(&self);
    /// Holds playback in place. The queue and position are untouched, unlike
    /// `stop`, which discards both.
    fn pause(&self);
    fn resume(&self);
    fn is_paused(&self) -> bool;
    /// Playback position within the utterance currently sounding, cumulative
    /// across that utterance's chunks but not across the queue. Reported in the
    /// sped-up output timeline; multiply by the playback speed to get a
    /// position on the original recording's clock.
    fn position(&self) -> Duration;
}

/// How long `pcm` sounds at 1x. Used to place chunk boundaries on the
/// utterance's own clock.
fn chunk_duration(pcm: &Pcm) -> Duration {
    let frames = pcm.samples.len() / (pcm.channels.max(1) as usize);
    if pcm.sample_rate == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(frames as f64 / pcm.sample_rate as f64)
}

/// One utterance's chunks, and how many of them have finished playing.
struct TrackedUtterance {
    chunk_durations: Vec<Duration>,
    finished_chunks: usize,
}

/// Maps appended chunks back to the utterances they belong to.
///
/// Both sinks append one source per chunk, but report one queue slot per
/// utterance. This holds the bookkeeping that makes those two views agree.
#[derive(Default)]
struct ChunkQueue {
    utterances: VecDeque<TrackedUtterance>,
    last_token: Option<u64>,
}

impl ChunkQueue {
    fn push(&mut self, token: u64, duration: Duration) {
        if self.last_token != Some(token) || self.utterances.is_empty() {
            self.utterances.push_back(TrackedUtterance {
                chunk_durations: Vec::new(),
                finished_chunks: 0,
            });
            self.last_token = Some(token);
        }
        if let Some(current) = self.utterances.back_mut() {
            current.chunk_durations.push(duration);
        }
    }

    fn clear(&mut self) {
        self.utterances.clear();
        self.last_token = None;
    }

    fn total_chunks(&self) -> usize {
        self.utterances
            .iter()
            .map(|utterance| utterance.chunk_durations.len())
            .sum()
    }

    /// Reconciles against how many appended chunks the underlying player still
    /// holds, dropping utterances that have fully drained and recording how far
    /// into the head utterance playback has reached.
    fn sync(&mut self, remaining_chunks: usize) {
        let mut drained = self.total_chunks().saturating_sub(remaining_chunks);
        while let Some(front) = self.utterances.front_mut() {
            if drained >= front.chunk_durations.len() {
                drained -= front.chunk_durations.len();
                self.utterances.pop_front();
            } else {
                front.finished_chunks = drained;
                break;
            }
        }
    }

    fn queued(&self) -> usize {
        self.utterances.len()
    }

    /// Where the currently sounding chunk starts on its utterance's clock,
    /// reported in the sped-up output timeline so it can be added to the
    /// underlying player's position without mixing clocks. Chunk durations are
    /// recorded at 1x, so they are divided by the playback speed here; the
    /// player multiplies the sum back up to read word timings.
    fn head_offset(&self, speed: f32) -> Duration {
        let elapsed: Duration = self
            .utterances
            .front()
            .map(|utterance| {
                utterance
                    .chunk_durations
                    .iter()
                    .take(utterance.finished_chunks)
                    .sum()
            })
            .unwrap_or(Duration::ZERO);
        if speed.is_finite() && speed > 0.0 {
            elapsed.div_f32(speed)
        } else {
            // A stopped clock makes no progress, so there is nothing to
            // rescale — and dividing by zero here would poison the position.
            elapsed
        }
    }
}

pub struct RodioSink {
    player: rodio::Player,
    /// Guards the chunk bookkeeping. Playback callbacks never touch it; only
    /// the player entity's thread does, so contention is nil.
    chunks: Mutex<ChunkQueue>,
    /// Mirror of the speed handed to rodio, needed to report chunk offsets on
    /// the same output clock rodio's own position uses.
    speed: Mutex<f32>,
}

impl RodioSink {
    pub fn new(player: rodio::Player) -> Self {
        Self {
            player,
            chunks: Mutex::new(ChunkQueue::default()),
            speed: Mutex::new(1.0),
        }
    }

    /// Reconciles the chunk bookkeeping with what rodio still holds, and runs
    /// `read` against it. Poisoning is unreachable in practice; a poisoned lock
    /// degrades to "nothing queued" rather than crashing playback.
    fn with_synced_chunks<R>(&self, read: impl FnOnce(&ChunkQueue) -> R, fallback: R) -> R {
        let Ok(mut chunks) = self.chunks.lock() else {
            log::error!("read_aloud: sink chunk queue poisoned");
            return fallback;
        };
        chunks.sync(self.player.len());
        read(&chunks)
    }

    fn speed(&self) -> f32 {
        self.speed.lock().map(|speed| *speed).unwrap_or(1.0)
    }
}

impl AudioSink for RodioSink {
    fn append_chunk(&self, utterance: u64, pcm: Pcm) {
        let Some(sample_rate) = std::num::NonZero::new(pcm.sample_rate) else {
            log::error!("read_aloud: refusing to play PCM with a zero sample rate");
            return;
        };
        let Some(channels) = std::num::NonZero::new(pcm.channels) else {
            log::error!("read_aloud: refusing to play PCM with zero channels");
            return;
        };
        let duration = chunk_duration(&pcm);
        // Recorded before the append so the queue never reports fewer chunks
        // than rodio holds, which `sync` would read as a phantom drain.
        match self.chunks.lock() {
            Ok(mut chunks) => {
                chunks.sync(self.player.len());
                chunks.push(utterance, duration);
            }
            Err(error) => log::error!("read_aloud: sink chunk queue poisoned: {error}"),
        }
        self.player.append(rodio::buffer::SamplesBuffer::new(
            channels,
            sample_rate,
            pcm.samples,
        ));
    }

    fn clear(&self) {
        // `Player::clear()` sets a counter of sounds to drop, then blocks the
        // calling thread in `sleep_until_end()` until the last appended
        // source finishes. On the GPUI main thread that is a permanent
        // freeze whenever nothing is ever going to finish naturally (e.g. we
        // just want to abandon the queue on seek). `skip_one()` bumps the
        // same drop counter without waiting, so draining the queue one
        // `skip_one()` per queued sound achieves the same result
        // non-blockingly; the sounds are discarded on the queue's next poll.
        for _ in 0..self.player.len() {
            self.player.skip_one();
        }
        if let Ok(mut chunks) = self.chunks.lock() {
            chunks.clear();
        }
        // `clear` leaves the player stopped; playback must be re-armed or the
        // next `append` is silent.
        self.player.play();
    }

    fn queued(&self) -> usize {
        self.with_synced_chunks(ChunkQueue::queued, 0)
    }

    fn set_speed(&self, speed: f32) {
        if let Ok(mut current) = self.speed.lock() {
            *current = speed;
        }
        self.player.set_speed(speed);
    }

    fn set_volume(&self, volume: f32) {
        self.player.set_volume(volume.clamp(0.0, 1.0));
    }

    fn stop(&self) {
        // `Player::stop()` sets a `stopped` flag; a later `append()` then
        // blocks (waiting for the flush its own doc comment describes)
        // until the flag is cleared, which only happens on `append()`'s own
        // call to `sleep_until_end()`. That is the same main-thread freeze
        // hazard as `clear()`, so drain the queue with `skip_one()` (see
        // `clear()` above) instead of setting the flag.
        for _ in 0..self.player.len() {
            self.player.skip_one();
        }
        if let Ok(mut chunks) = self.chunks.lock() {
            chunks.clear();
        }
        self.player.pause();
    }

    fn pause(&self) {
        self.player.pause();
    }

    fn resume(&self) {
        self.player.play();
    }

    fn is_paused(&self) -> bool {
        self.player.is_paused()
    }

    fn position(&self) -> Duration {
        // Non-blocking: `get_pos` is a mutex read of a value the audio
        // thread refreshes every 5ms. It restarts at zero for every appended
        // source, so a multi-chunk utterance needs the already-played chunks
        // added back to keep the position on the utterance's own clock.
        let speed = self.speed();
        let offset = self.with_synced_chunks(|chunks| chunks.head_offset(speed), Duration::ZERO);
        offset + self.player.get_pos()
    }
}

#[derive(Default)]
struct FakeSinkState {
    /// One entry per queued utterance, holding that utterance's chunks. A
    /// single-chunk utterance — what `FakeTts` produces — behaves exactly as
    /// the pre-streaming queue did, so the player's tests keep their meaning.
    queue: VecDeque<Vec<Pcm>>,
    last_token: Option<u64>,
    /// Chunks of the head utterance that have finished playing.
    finished_chunks: usize,
    speed: f32,
    volume: f32,
    stopped: bool,
    paused: bool,
    position: Duration,
}

#[derive(Clone)]
pub struct FakeSink {
    state: Arc<Mutex<FakeSinkState>>,
}

impl FakeSink {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeSinkState {
                queue: VecDeque::new(),
                last_token: None,
                finished_chunks: 0,
                speed: 1.0,
                volume: 1.0,
                stopped: false,
                paused: false,
                position: Duration::ZERO,
            })),
        }
    }

    /// The loudness the reader last asked for, so a test can tell ducking
    /// from stopping.
    pub fn volume(&self) -> f32 {
        self.state.lock().map(|state| state.volume).unwrap_or(1.0)
    }

    /// Simulates the head *utterance* finishing playback, however many chunks
    /// it was streamed in. The position restarts at zero for the next
    /// utterance, as rodio's does for the next source.
    pub fn finish_one(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.queue.pop_front();
            state.finished_chunks = 0;
            state.position = Duration::ZERO;
        }
    }

    /// Simulates one *chunk* of the head utterance finishing, leaving the rest
    /// of that utterance queued. Finishing the last chunk retires the
    /// utterance, the same way rodio draining its final source does.
    pub fn finish_chunk(&self) {
        if let Ok(mut state) = self.state.lock() {
            let head_chunks = state.queue.front().map(Vec::len).unwrap_or(0);
            if state.finished_chunks + 1 >= head_chunks {
                state.queue.pop_front();
                state.finished_chunks = 0;
            } else {
                state.finished_chunks += 1;
            }
            state.position = Duration::ZERO;
        }
    }

    /// Number of chunks queued across every utterance, for asserting that
    /// streaming actually appended more than one buffer.
    pub fn queued_chunks(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.queue.iter().map(Vec::len).sum())
            .unwrap_or(0)
    }

    /// Simulates playback progressing within the current head of the queue.
    pub fn set_position(&self, position: Duration) {
        if let Ok(mut state) = self.state.lock() {
            state.position = position;
        }
    }

    pub fn speed(&self) -> f32 {
        self.state.lock().map(|state| state.speed).unwrap_or(1.0)
    }

    pub fn is_stopped(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.stopped)
            .unwrap_or(false)
    }
}

impl Default for FakeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSink for FakeSink {
    fn append_chunk(&self, utterance: u64, pcm: Pcm) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = false;
            if state.last_token != Some(utterance) || state.queue.is_empty() {
                state.queue.push_back(Vec::new());
                state.last_token = Some(utterance);
            }
            if let Some(current) = state.queue.back_mut() {
                current.push(pcm);
            }
        }
    }

    fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.queue.clear();
            state.last_token = None;
            state.finished_chunks = 0;
            state.position = Duration::ZERO;
        }
    }

    fn queued(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.queue.len())
            .unwrap_or(0)
    }

    fn set_speed(&self, speed: f32) {
        if let Ok(mut state) = self.state.lock() {
            state.speed = speed;
        }
    }

    fn set_volume(&self, volume: f32) {
        if let Ok(mut state) = self.state.lock() {
            state.volume = volume;
        }
    }

    fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.queue.clear();
            state.last_token = None;
            state.finished_chunks = 0;
            state.stopped = true;
            state.position = Duration::ZERO;
        }
    }

    fn pause(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.paused = true;
        }
    }

    fn resume(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.paused = false;
        }
    }

    fn is_paused(&self) -> bool {
        self.state.lock().map(|state| state.paused).unwrap_or(false)
    }

    fn position(&self) -> Duration {
        self.state
            .lock()
            .map(|state| {
                let elapsed: Duration = state
                    .queue
                    .front()
                    .map(|chunks| {
                        chunks
                            .iter()
                            .take(state.finished_chunks)
                            .map(chunk_duration)
                            .sum()
                    })
                    .unwrap_or(Duration::ZERO);
                let offset = if state.speed.is_finite() && state.speed > 0.0 {
                    elapsed.div_f32(state.speed)
                } else {
                    elapsed
                };
                offset + state.position
            })
            .unwrap_or(Duration::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(sample_count: usize) -> Pcm {
        Pcm {
            samples: vec![0.0; sample_count],
            sample_rate: 22050,
            channels: 1,
            words: Vec::new(),
        }
    }

    #[test]
    fn fake_sink_tracks_queue_depth() {
        let sink = FakeSink::new();
        assert_eq!(sink.queued(), 0);
        sink.append_chunk(0, pcm(4));
        sink.append_chunk(1, pcm(4));
        assert_eq!(sink.queued(), 2);
    }

    #[test]
    fn fake_sink_clear_empties_the_queue() {
        let sink = FakeSink::new();
        sink.append_chunk(0, pcm(4));
        sink.clear();
        assert_eq!(sink.queued(), 0);
    }

    #[test]
    fn fake_sink_can_finish_one_item_at_a_time() {
        let sink = FakeSink::new();
        sink.append_chunk(0, pcm(1));
        sink.append_chunk(1, pcm(2));
        sink.finish_one();
        assert_eq!(sink.queued(), 1);
        sink.finish_one();
        assert_eq!(sink.queued(), 0);
    }

    #[test]
    fn fake_sink_position_is_settable_and_resets_when_a_source_ends() {
        let sink = FakeSink::new();
        sink.append_chunk(0, pcm(4));
        sink.set_position(Duration::from_millis(250));
        assert_eq!(sink.position(), Duration::from_millis(250));
        sink.finish_one();
        assert_eq!(
            sink.position(),
            Duration::ZERO,
            "the next source starts its own clock, as rodio's does"
        );
    }

    /// One second of mono audio at the rate `pcm()` uses.
    fn one_second() -> Pcm {
        Pcm {
            samples: vec![0.0; 22050],
            sample_rate: 22050,
            channels: 1,
            words: Vec::new(),
        }
    }

    #[test]
    fn chunks_sharing_a_token_are_one_queued_utterance() {
        let sink = FakeSink::new();
        sink.append_chunk(7, pcm(4));
        sink.append_chunk(7, pcm(4));
        sink.append_chunk(8, pcm(4));

        assert_eq!(sink.queued(), 2, "two tokens means two utterances");
        assert_eq!(sink.queued_chunks(), 3);

        sink.finish_one();
        assert_eq!(
            sink.queued(),
            1,
            "finishing an utterance retires all of its chunks"
        );
        assert_eq!(sink.queued_chunks(), 1);
    }

    /// The position the player reads must be cumulative within an utterance,
    /// because the word timings it compares against are timed from the
    /// utterance's start while the underlying player restarts per chunk.
    #[test]
    fn position_accumulates_finished_chunks_of_the_current_utterance() {
        let sink = FakeSink::new();
        sink.append_chunk(0, one_second());
        sink.append_chunk(0, one_second());

        sink.set_position(Duration::from_millis(250));
        assert_eq!(
            sink.position(),
            Duration::from_millis(250),
            "nothing has finished yet, so the position is the raw one"
        );

        sink.finish_chunk();
        sink.set_position(Duration::from_millis(100));
        assert_eq!(
            sink.position(),
            Duration::from_millis(1100),
            "the finished second of audio has to count toward the utterance"
        );
    }

    /// Chunk durations are recorded at 1x but `position()` is contracted to the
    /// sped-up output clock, so the offset has to be rescaled — otherwise the
    /// karaoke highlight drifts further ahead with every chunk.
    #[test]
    fn the_chunk_offset_is_reported_on_the_output_clock() {
        let sink = FakeSink::new();
        sink.set_speed(2.0);
        sink.append_chunk(0, one_second());
        sink.append_chunk(0, one_second());

        sink.finish_chunk();
        sink.set_position(Duration::ZERO);
        assert_eq!(
            sink.position(),
            Duration::from_millis(500),
            "a second of audio at 2x occupies half a second of output"
        );
    }

    #[test]
    fn a_standstill_speed_does_not_poison_the_position() {
        let sink = FakeSink::new();
        sink.set_speed(0.0);
        sink.append_chunk(0, one_second());
        sink.append_chunk(0, one_second());

        sink.finish_chunk();
        sink.set_position(Duration::ZERO);
        assert_eq!(
            sink.position(),
            Duration::from_secs(1),
            "dividing by a stopped clock must not produce an infinite position"
        );
    }

    #[test]
    fn fake_sink_records_speed_changes() {
        let sink = FakeSink::new();
        sink.set_speed(1.5);
        assert_eq!(sink.speed(), 1.5);
    }

    #[test]
    fn fake_sink_pause_holds_the_queue_and_resume_releases_it() {
        let sink = FakeSink::new();
        sink.append_chunk(0, pcm(4));
        sink.pause();
        assert!(sink.is_paused());
        assert_eq!(sink.queued(), 1, "pause must not discard queued audio");
        sink.resume();
        assert!(!sink.is_paused());
        assert_eq!(sink.queued(), 1);
    }
}
