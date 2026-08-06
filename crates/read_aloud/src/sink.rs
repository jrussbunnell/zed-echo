use crate::provider::Pcm;
use std::sync::{Arc, Mutex};

/// Playback seam. `RodioSink` is the real one; `FakeSink` keeps the player's
/// tests free of any dependency on a sound card.
pub trait AudioSink: 'static {
    fn append(&self, pcm: Pcm);
    fn clear(&self);
    /// Number of utterances still queued, including the one playing.
    fn queued(&self) -> usize;
    fn set_speed(&self, speed: f32);
    fn stop(&self);
    /// Holds playback in place. The queue and position are untouched, unlike
    /// `stop`, which discards both.
    fn pause(&self);
    fn resume(&self);
    fn is_paused(&self) -> bool;
}

pub struct RodioSink(rodio::Player);

impl RodioSink {
    pub fn new(player: rodio::Player) -> Self {
        Self(player)
    }
}

impl AudioSink for RodioSink {
    fn append(&self, pcm: Pcm) {
        let Some(sample_rate) = std::num::NonZero::new(pcm.sample_rate) else {
            log::error!("read_aloud: refusing to play PCM with a zero sample rate");
            return;
        };
        let Some(channels) = std::num::NonZero::new(pcm.channels) else {
            log::error!("read_aloud: refusing to play PCM with zero channels");
            return;
        };
        self.0.append(rodio::buffer::SamplesBuffer::new(
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
        for _ in 0..self.0.len() {
            self.0.skip_one();
        }
        // `clear` leaves the player stopped; playback must be re-armed or the
        // next `append` is silent.
        self.0.play();
    }

    fn queued(&self) -> usize {
        self.0.len()
    }

    fn set_speed(&self, speed: f32) {
        self.0.set_speed(speed);
    }

    fn stop(&self) {
        // `Player::stop()` sets a `stopped` flag; a later `append()` then
        // blocks (waiting for the flush its own doc comment describes)
        // until the flag is cleared, which only happens on `append()`'s own
        // call to `sleep_until_end()`. That is the same main-thread freeze
        // hazard as `clear()`, so drain the queue with `skip_one()` (see
        // `clear()` above) instead of setting the flag.
        for _ in 0..self.0.len() {
            self.0.skip_one();
        }
        self.0.pause();
    }

    fn pause(&self) {
        self.0.pause();
    }

    fn resume(&self) {
        self.0.play();
    }

    fn is_paused(&self) -> bool {
        self.0.is_paused()
    }
}

#[derive(Default)]
struct FakeSinkState {
    queue: Vec<Pcm>,
    speed: f32,
    stopped: bool,
    paused: bool,
}

#[derive(Clone)]
pub struct FakeSink {
    state: Arc<Mutex<FakeSinkState>>,
}

impl FakeSink {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeSinkState {
                queue: Vec::new(),
                speed: 1.0,
                stopped: false,
                paused: false,
            })),
        }
    }

    /// Simulates the head of the queue finishing playback.
    pub fn finish_one(&self) {
        if let Ok(mut state) = self.state.lock() {
            if !state.queue.is_empty() {
                state.queue.remove(0);
            }
        }
    }

    pub fn speed(&self) -> f32 {
        self.state.lock().map(|state| state.speed).unwrap_or(1.0)
    }

    pub fn is_stopped(&self) -> bool {
        self.state.lock().map(|state| state.stopped).unwrap_or(false)
    }
}

impl Default for FakeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSink for FakeSink {
    fn append(&self, pcm: Pcm) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = false;
            state.queue.push(pcm);
        }
    }

    fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.queue.clear();
        }
    }

    fn queued(&self) -> usize {
        self.state.lock().map(|state| state.queue.len()).unwrap_or(0)
    }

    fn set_speed(&self, speed: f32) {
        if let Ok(mut state) = self.state.lock() {
            state.speed = speed;
        }
    }

    fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.queue.clear();
            state.stopped = true;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(sample_count: usize) -> Pcm {
        Pcm {
            samples: vec![0.0; sample_count],
            sample_rate: 22050,
            channels: 1,
        }
    }

    #[test]
    fn fake_sink_tracks_queue_depth() {
        let sink = FakeSink::new();
        assert_eq!(sink.queued(), 0);
        sink.append(pcm(4));
        sink.append(pcm(4));
        assert_eq!(sink.queued(), 2);
    }

    #[test]
    fn fake_sink_clear_empties_the_queue() {
        let sink = FakeSink::new();
        sink.append(pcm(4));
        sink.clear();
        assert_eq!(sink.queued(), 0);
    }

    #[test]
    fn fake_sink_can_finish_one_item_at_a_time() {
        let sink = FakeSink::new();
        sink.append(pcm(1));
        sink.append(pcm(2));
        sink.finish_one();
        assert_eq!(sink.queued(), 1);
        sink.finish_one();
        assert_eq!(sink.queued(), 0);
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
        sink.append(pcm(4));
        sink.pause();
        assert!(sink.is_paused());
        assert_eq!(sink.queued(), 1, "pause must not discard queued audio");
        sink.resume();
        assert!(!sink.is_paused());
        assert_eq!(sink.queued(), 1);
    }
}
