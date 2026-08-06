use crate::provider::TtsProvider;
use crate::segmenter::Utterance;
use crate::sink::AudioSink;
use gpui::{Context, Entity, EventEmitter, Task};
use std::sync::Arc;
use util::ResultExt as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerEvent {
    Speaking(usize),
    Finished,
}

pub struct Player {
    provider: Arc<dyn TtsProvider>,
    sink: Box<dyn AudioSink>,
    utterances: Vec<Utterance>,
    /// Index of the next utterance to synthesize.
    next_to_synthesize: usize,
    /// Index of the utterance at the head of the sink's queue.
    queue_head: usize,
    last_reported: Option<usize>,
    synthesis: Option<Task<()>>,
}

impl EventEmitter<PlayerEvent> for Player {}

impl Player {
    pub fn new(
        provider: Arc<dyn TtsProvider>,
        sink: Box<dyn AudioSink>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            provider,
            sink,
            utterances: Vec::new(),
            next_to_synthesize: 0,
            queue_head: 0,
            last_reported: None,
            synthesis: None,
        }
    }

    /// Replaces the utterance list. Utterances already synthesized keep their
    /// place, so a streaming append only synthesizes what is new.
    pub fn set_utterances(&mut self, utterances: Vec<Utterance>, cx: &mut Context<Self>) {
        self.utterances = utterances;
        if self.next_to_synthesize > self.utterances.len() {
            self.next_to_synthesize = self.utterances.len();
        }
        self.pump(cx);
    }

    pub fn seek_to(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.utterances.len() {
            return;
        }
        self.sink.clear();
        self.queue_head = index;
        self.next_to_synthesize = index;
        self.last_reported = None;
        self.pump(cx);
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.synthesis = None;
        self.sink.stop();
        self.queue_head = self.utterances.len();
        self.next_to_synthesize = self.utterances.len();
        self.last_reported = None;
        cx.emit(PlayerEvent::Finished);
        cx.notify();
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.sink.set_speed(speed);
    }

    pub fn pause(&mut self) {
        self.sink.pause();
    }

    pub fn resume(&mut self) {
        self.sink.resume();
    }

    pub fn is_paused(&self) -> bool {
        self.sink.is_paused()
    }

    pub fn speaking_index(&self) -> Option<usize> {
        if self.sink.queued() == 0 {
            return None;
        }
        (self.queue_head < self.utterances.len()).then_some(self.queue_head)
    }

    pub fn utterances(&self) -> &[Utterance] {
        &self.utterances
    }

    /// Recomputes the speaking index from how far the sink has drained.
    /// Called on a timer by the owning entity, and directly in tests.
    pub fn poll_position(&mut self, cx: &mut Context<Self>) {
        let queued = self.sink.queued();
        let head = self.next_to_synthesize.saturating_sub(queued);
        if head != self.queue_head {
            self.queue_head = head;
        }

        let speaking = self.speaking_index();
        if speaking != self.last_reported {
            self.last_reported = speaking;
            match speaking {
                Some(index) => cx.emit(PlayerEvent::Speaking(index)),
                None => cx.emit(PlayerEvent::Finished),
            }
            cx.notify();
        }
    }

    /// Keeps one utterance synthesized ahead of the one playing.
    fn pump(&mut self, cx: &mut Context<Self>) {
        const PREFETCH: usize = 2;

        if self.synthesis.is_some() {
            return;
        }
        if self.next_to_synthesize >= self.utterances.len() {
            return;
        }
        if self.sink.queued() >= PREFETCH {
            return;
        }

        let index = self.next_to_synthesize;
        let Some(utterance) = self.utterances.get(index) else {
            return;
        };
        let text = utterance.spoken_text.clone();
        let provider = self.provider.clone();

        self.synthesis = Some(cx.spawn(async move |this, cx| {
            let synthesized = cx.update(|cx| provider.synthesize(text, cx)).await;

            this.update(cx, |this, cx| {
                this.synthesis = None;
                this.next_to_synthesize = this.next_to_synthesize.max(index + 1);
                match synthesized {
                    Ok(pcm) => this.sink.append(pcm),
                    Err(error) => {
                        // A failed utterance is skipped, never retried, and never
                        // allowed to block the ones behind it.
                        log::warn!("read_aloud: synthesis failed, skipping: {error:#}");
                    }
                }
                this.poll_position(cx);
                this.pump(cx);
            })
            .log_err();
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FakeTts;
    use crate::sink::FakeSink;
    use gpui::{AppContext as _, TestAppContext};

    fn utterance(text: &str, start: usize) -> Utterance {
        Utterance {
            source_range: start..start + text.len(),
            spoken_text: text.to_string(),
        }
    }

    fn setup(cx: &mut TestAppContext) -> (Entity<Player>, FakeTts, FakeSink) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let player = cx.new({
            let provider = provider.clone();
            let sink = sink.clone();
            |cx| Player::new(Arc::new(provider), Box::new(sink), cx)
        });
        (player, provider, sink)
    }

    #[gpui::test]
    async fn synthesizes_in_queue_order(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("First.", 0), utterance("Second.", 7)],
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(provider.spoken(), vec!["First.", "Second."]);
    }

    #[gpui::test]
    async fn reports_the_speaking_index(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("One.", 0), utterance("Two.", 5)],
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(player.read_with(cx, |player, _| player.speaking_index()), Some(0));

        sink.finish_one();
        player.update(cx, |player, cx| player.poll_position(cx));
        assert_eq!(player.read_with(cx, |player, _| player.speaking_index()), Some(1));
    }

    #[gpui::test]
    async fn seek_discards_the_queue_and_resumes_from_the_target(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("A.", 0), utterance("B.", 3), utterance("C.", 6)],
                cx,
            );
        });
        cx.run_until_parked();

        player.update(cx, |player, cx| player.seek_to(2, cx));
        cx.run_until_parked();

        assert_eq!(player.read_with(cx, |player, _| player.speaking_index()), Some(2));
        assert_eq!(sink.queued(), 1, "seek must discard everything before the target");
        assert_eq!(
            provider.spoken().last().map(String::as_str),
            Some("C."),
            "seek must synthesize the target utterance"
        );
    }

    #[gpui::test]
    async fn a_synthesis_failure_does_not_stall_the_queue(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        provider.fail_next();
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("Doomed.", 0), utterance("Survivor.", 8)],
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            provider.spoken(),
            vec!["Survivor."],
            "the failed utterance is skipped and the queue keeps moving"
        );
    }

    #[gpui::test]
    async fn stop_clears_everything(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("A.", 0), utterance("B.", 3)], cx);
        });
        cx.run_until_parked();

        player.update(cx, |player, cx| player.stop(cx));
        assert!(sink.is_stopped());
        assert_eq!(player.read_with(cx, |player, _| player.speaking_index()), None);
    }

    #[gpui::test]
    async fn appending_utterances_does_not_resynthesize_earlier_ones(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("First.", 0)], cx);
        });
        cx.run_until_parked();

        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("First.", 0), utterance("Second.", 7)],
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            provider.spoken(),
            vec!["First.", "Second."],
            "streaming appends must not re-speak what was already queued"
        );
    }

    #[gpui::test]
    async fn speed_is_forwarded_to_the_sink(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, _cx| player.set_speed(1.25));
        assert_eq!(sink.speed(), 1.25);
    }

    #[gpui::test]
    async fn pause_holds_position_and_resume_continues(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("One.", 0), utterance("Two.", 5)], cx);
        });
        cx.run_until_parked();

        player.update(cx, |player, _cx| player.pause());
        assert!(sink.is_paused());
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(0),
            "pause must not lose the speaking position"
        );

        player.update(cx, |player, _cx| player.resume());
        assert!(!sink.is_paused());
        assert_eq!(player.read_with(cx, |player, _| player.speaking_index()), Some(0));
    }
}
