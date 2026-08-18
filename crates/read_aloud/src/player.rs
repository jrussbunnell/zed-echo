use crate::provider::{TtsProvider, WordTiming};
use crate::segmenter::Utterance;
use crate::sink::AudioSink;
use futures::StreamExt as _;
use gpui::{App, Context, EventEmitter, Task};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use util::ResultExt as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerEvent {
    Speaking(usize),
    Finished,
}

/// Builds a replacement sink when the one in use has stopped being audible —
/// the output device moved, or its stream broke. `None` means the sink in use
/// is still the right one.
///
/// The `bool` forces a replacement even when the output looks fine from the
/// outside, for when playback itself can tell it is not sounding.
///
/// Injected rather than reached for directly so this crate keeps knowing
/// nothing about sound cards, which is what lets its tests run without one.
pub type SinkRecovery = Arc<dyn Fn(&mut App, bool) -> Option<Box<dyn AudioSink>>>;

/// How long playback may make no progress — audio queued, not paused, and
/// neither the position nor the queue moving — before the output stream is
/// treated as wedged and reopened.
///
/// Long enough that a slow first chunk or a scheduling hiccup never trips it,
/// short enough that a listener hears a gap rather than the rest of the
/// message in silence.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);

/// How many times playback may reopen a stalled output stream before giving up.
///
/// Reopening re-speaks the current utterance, so a device that is silent no
/// matter what would otherwise loop: a gap, a resynthesis (a billed TTS
/// request), silence again. Two retries covers a stream that broke; past that,
/// the problem is not the stream.
const MAX_STALL_RECOVERIES: usize = 2;

pub struct Player {
    provider: Arc<dyn TtsProvider>,
    sink: Box<dyn AudioSink>,
    recover_sink: Option<SinkRecovery>,
    utterances: Vec<Utterance>,
    /// Index of the next utterance to synthesize.
    next_to_synthesize: usize,
    last_reported: Option<usize>,
    synthesis: Option<Task<()>>,
    /// Word timings per utterance index, resolved to source ranges when the
    /// synthesized audio arrives so the per-tick position lookup stays cheap.
    word_timings: HashMap<usize, Vec<TimedWord>>,
    /// Mirror of the speed last handed to the sink. The sink's position runs
    /// on the sped-up output clock, so word timings — which are on the
    /// recording's clock — need the position multiplied back up by this.
    speed: f32,
    /// Identifies each streamed utterance to the sink, so it can tell one
    /// utterance's chunks from the next one's. Monotonic rather than the
    /// utterance index, which repeats across seeks and resets.
    next_utterance_token: u64,
    /// Whether the listener paused, as opposed to the sink being left stopped
    /// by an abandoned playback. Appending to a stopped sink is silent, so the
    /// two have to be told apart before re-arming it.
    paused_by_user: bool,
    /// Last (queued utterances, position) seen to differ, and when — the basis
    /// for noticing that the output stream has stopped sounding. See
    /// [`STALL_TIMEOUT`].
    last_progress: Option<((usize, Duration), Instant)>,
    /// Stalls recovered from without playback moving since, bounded by
    /// [`MAX_STALL_RECOVERIES`].
    stall_recoveries: usize,
}

/// A word timing resolved against the utterance's spoken text. `source_range`
/// is `None` when the spoken word could not be matched back to the source; the
/// word's slot in the timeline is kept anyway so its neighbors stay aligned.
struct TimedWord {
    start: Duration,
    source_range: Option<Range<usize>>,
}

impl EventEmitter<PlayerEvent> for Player {}

impl Player {
    pub fn new(
        provider: Arc<dyn TtsProvider>,
        sink: Box<dyn AudioSink>,
        recover_sink: Option<SinkRecovery>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            provider,
            sink,
            recover_sink,
            utterances: Vec::new(),
            next_to_synthesize: 0,
            last_reported: None,
            synthesis: None,
            word_timings: HashMap::new(),
            speed: 1.0,
            next_utterance_token: 0,
            paused_by_user: false,
            last_progress: None,
            stall_recoveries: 0,
        }
    }

    /// Replaces the utterance list. Utterances already synthesized keep their
    /// place, so a streaming append only synthesizes what is new.
    pub fn set_utterances(&mut self, utterances: Vec<Utterance>, cx: &mut Context<Self>) {
        if self.utterances != utterances {
            self.utterances = utterances;
            // The utterance count feeds UI (the mini player's counter), which
            // observes this entity rather than polling it.
            cx.notify();
        }
        if self.next_to_synthesize > self.utterances.len() {
            self.next_to_synthesize = self.utterances.len();
        }
        self.pump(cx);
    }

    pub fn seek_to(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.utterances.len() {
            return;
        }
        // Drop any in-flight synthesis before touching the sink. Otherwise
        // that task is still holding `index + 1` worth of stale target, and
        // when it finishes it appends pre-seek audio into the post-seek
        // queue and pulls `next_to_synthesize` back down, snapping playback
        // back to before the seek.
        self.synthesis = None;
        self.sink.clear();
        self.next_to_synthesize = index;
        self.last_reported = None;
        self.pump(cx);
        cx.notify();
    }

    /// Cancels in-flight synthesis, drops whatever audio is queued, and
    /// restarts synthesis from the top of the current utterance list —
    /// including when that list is currently empty (unlike `seek_to`, which
    /// early-returns if the target index is out of range and so cannot be
    /// used to discard stale audio while the new content has nothing to
    /// speak yet). Emits no event: `last_reported` becomes `None`, so the
    /// next `poll_position` naturally emits `Speaking`/`Finished` for
    /// whatever the recomputed position turns out to be, even if that
    /// position happens to be the same index as before the reset.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.synthesis = None;
        self.sink.clear();
        self.next_to_synthesize = 0;
        self.last_reported = None;
        self.word_timings.clear();
        self.pump(cx);
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.synthesis = None;
        self.paused_by_user = false;
        self.last_progress = None;
        self.sink.stop();
        self.next_to_synthesize = self.utterances.len();
        self.last_reported = None;
        self.word_timings.clear();
        cx.emit(PlayerEvent::Finished);
        cx.notify();
    }

    pub fn set_speed(&mut self, speed: f32) {
        /// Far past any usable speaking rate; only there to bound the math.
        const MAX_SPEED: f32 = 10.0;
        // Clamped because it scales a `Duration` in the word-position math,
        // which panics on negative, NaN, or overflowing factors — and
        // `speaking_rate` arrives unvalidated from user settings. The sink
        // gets the same clamped value so the audible speed and the highlight
        // clock can never disagree.
        self.speed = if speed.is_finite() {
            speed.clamp(0.0, MAX_SPEED)
        } else {
            1.0
        };
        self.sink.set_speed(self.speed);
    }

    pub fn pause(&mut self) {
        self.paused_by_user = true;
        self.sink.pause();
    }

    pub fn resume(&mut self) {
        self.paused_by_user = false;
        self.sink.resume();
        // A paused stretch is not a stalled one; start the stall window over.
        self.last_progress = None;
    }

    pub fn is_paused(&self) -> bool {
        self.sink.is_paused()
    }

    /// Derived, not tracked: `queued() == 0` means playback caught up to
    /// everything synthesized so far, and how far behind `next_to_synthesize`
    /// the queue's drained to gives the utterance actually sounding right
    /// now. Deriving this on every read — instead of caching it — means
    /// there is no separate field that a caller like `seek_to` could forget
    /// to update, which is what let the queue and the reported position
    /// drift apart before.
    pub fn speaking_index(&self) -> Option<usize> {
        let queued = self.sink.queued();
        if queued == 0 {
            return None;
        }
        let head = self.next_to_synthesize.saturating_sub(queued);
        (head < self.utterances.len()).then_some(head)
    }

    pub fn utterances(&self) -> &[Utterance] {
        &self.utterances
    }

    /// True when there is nothing left to do: no audio queued, no synthesis
    /// in flight, and nothing awaiting synthesis. Unlike `speaking_index()`
    /// being `None` — which also happens transiently whenever playback
    /// drains faster than synthesis or a slow stream delivers text — this is
    /// the durable "done until someone hands over more work" state.
    pub fn is_idle(&self) -> bool {
        self.sink.queued() == 0
            && self.synthesis.is_none()
            && self.next_to_synthesize >= self.utterances.len()
    }

    /// Index of the utterance currently being synthesized (or next in line).
    /// Stands in for `speaking_index()` in UI while the sink is momentarily
    /// empty but work is still under way.
    pub fn next_to_synthesize(&self) -> usize {
        self.next_to_synthesize
    }

    /// Source range of the word being spoken right now, derived from the
    /// sink's playback position within the current utterance. `None` when
    /// nothing is playing, the provider supplied no timings, playback has not
    /// reached the first word yet, or the current word could not be matched
    /// back to the source — callers then fall back to the sentence highlight.
    pub fn current_word_source_range(&self) -> Option<Range<usize>> {
        let index = self.speaking_index()?;
        let words = self.word_timings.get(&index)?;
        // The sink's position runs on the sped-up output clock and restarts
        // for every queued utterance; scaling by the speed puts it back on
        // the recording's clock, which is what the timings use.
        let elapsed = self.sink.position().mul_f32(self.speed);
        // The most recently started word stays lit until the next one starts,
        // so inter-word gaps and trailing silence never flash the highlight
        // off. Before the first word starts, only the sentence wash shows.
        let started = words.partition_point(|word| word.start <= elapsed);
        words.get(started.checked_sub(1)?)?.source_range.clone()
    }

    /// Moves playback onto a fresh sink when the one in use has gone silent
    /// under us, in either of the two shapes that has:
    ///
    /// - the output device moved, leaving the old stream bound to a device the
    ///   listener is no longer on — it keeps draining, so the highlights keep
    ///   advancing over audio nobody can hear;
    /// - the stream stopped sounding at all, which cpal does not always report
    ///   as an error, and which no other code path here would ever undo — the
    ///   state that only quitting the app used to clear.
    ///
    /// The audio already handed to the old sink cannot be moved, so the
    /// utterance that was sounding is spoken again from its start on the new
    /// device rather than resuming mid-sentence.
    fn recover_sink_if_silent(&mut self, cx: &mut Context<Self>) {
        let Some(recover_sink) = self.recover_sink.clone() else {
            return;
        };
        // Nothing playing: leave the switch to the next utterance, which opens
        // the current device anyway.
        if self.is_idle() {
            return;
        }
        let stalled = self.playback_has_stalled(cx);
        let resume_at = self.speaking_index().unwrap_or(self.next_to_synthesize);
        let Some(sink) = recover_sink(cx, stalled) else {
            return;
        };
        if stalled {
            self.stall_recoveries += 1;
            log::warn!(
                "read_aloud: output stream stopped sounding; reopening and resuming from utterance {resume_at}"
            );
        } else {
            log::info!("read_aloud: moving playback onto a new output device");
        }
        self.sink = sink;
        self.sink.set_speed(self.speed);
        self.synthesis = None;
        self.last_reported = None;
        self.last_progress = None;
        self.seek_to(resume_at, cx);
    }

    /// Whether audio that should be sounding has stopped moving: queued, not
    /// paused, and neither the position nor the queue changing for
    /// [`STALL_TIMEOUT`].
    ///
    /// This is the shape of a wedged output stream, which cpal does not always
    /// report as an error — a device that went away behind an unchanged id, or a
    /// virtual device that accepts samples and plays nothing. Waiting on
    /// synthesis looks nothing like it: the queue is empty then.
    fn playback_has_stalled(&mut self, cx: &Context<Self>) -> bool {
        if self.sink.is_paused() || self.sink.queued() == 0 {
            self.last_progress = None;
            return false;
        }
        let now = cx.background_executor().now();
        let progress = (self.sink.queued(), self.sink.position());
        match self.last_progress {
            Some((last, since)) if last == progress => {
                now.saturating_duration_since(since) >= STALL_TIMEOUT
                    && self.stall_recoveries < MAX_STALL_RECOVERIES
            }
            _ => {
                // Playback moved, so whatever went wrong before is behind us.
                self.stall_recoveries = 0;
                self.last_progress = Some((progress, now));
                false
            }
        }
    }

    /// Recomputes the speaking index from how far the sink has drained, and
    /// keeps synthesis running ahead of it. Called on a timer by the owning
    /// entity, and directly in tests.
    pub fn poll_position(&mut self, cx: &mut Context<Self>) {
        self.recover_sink_if_silent(cx);
        let speaking = self.speaking_index();
        if speaking != self.last_reported {
            self.last_reported = speaking;
            match speaking {
                Some(index) => cx.emit(PlayerEvent::Speaking(index)),
                None => cx.emit(PlayerEvent::Finished),
            }
            cx.notify();
        }
        // The sink only drains as playback proceeds; nothing else prods
        // synthesis to refill it, so without this a document longer than
        // `PREFETCH` idles forever once the initial prefetch is consumed.
        self.pump(cx);
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
        // `stop` leaves the sink paused, and appending to a paused sink is
        // silent. Anything the listener did not pause has to be re-armed here
        // or the next message plays to nobody.
        if !self.paused_by_user && self.sink.is_paused() {
            self.sink.resume();
        }
        let text = utterance.spoken_text.clone();
        let provider = self.provider.clone();
        // A token rather than the index: seeking and resetting can queue the
        // same index again, and the sink must not glue the new audio onto the
        // old utterance's tail.
        let token = self.next_utterance_token;
        self.next_utterance_token += 1;

        self.synthesis = Some(cx.spawn(async move |this, cx| {
            let mut chunks = cx.update(|cx| provider.synthesize(text, cx));

            // Each chunk is appended as it lands, so playback starts on the
            // first one instead of waiting out the whole utterance. Advancing
            // `next_to_synthesize` is deferred to the end: it is what
            // `speaking_index` measures the queue against, and moving it while
            // chunks are still arriving would report the next utterance as
            // already speaking.
            while let Some(chunk) = chunks.next().await {
                let landed = this
                    .update(cx, |this, cx| match chunk {
                        Ok(pcm) => {
                            let timed = this
                                .utterances
                                .get(index)
                                .map(|utterance| align_word_timings(utterance, &pcm.words))
                                .unwrap_or_default();
                            if timed.is_empty() {
                                this.word_timings.remove(&index);
                            } else {
                                this.word_timings.insert(index, timed);
                            }
                            this.sink.append_chunk(token, pcm);
                            cx.notify();
                            true
                        }
                        Err(error) => {
                            // A failed utterance is skipped, never retried, and
                            // never allowed to block the ones behind it. A
                            // failure partway through keeps the chunks that
                            // already landed rather than cutting them off.
                            log::warn!("read_aloud: synthesis failed, skipping: {error:#}");
                            false
                        }
                    })
                    .log_err();
                match landed {
                    Some(true) => {}
                    // The utterance failed partway: stop pulling chunks for it.
                    Some(false) => break,
                    // The player entity is gone; nothing left to update.
                    None => return,
                }
            }

            this.update(cx, |this, cx| {
                this.synthesis = None;
                this.next_to_synthesize = this.next_to_synthesize.max(index + 1);
                // `poll_position` re-pumps once the position settles.
                this.poll_position(cx);
            })
            .log_err();
        }));
    }
}

/// Matches the provider's spoken-word list against the utterance's
/// whitespace-separated tokens, resolving each timing to a source range.
///
/// The provider normalizes text before speaking it (case and punctuation
/// shift, numbers become words, and Inworld emits whitespace/punctuation
/// tokens of their own), so this matcher is deliberately tolerant: it
/// compares only lowercased alphanumeric content and, on a mismatch, looks a
/// few words ahead on both sides to resynchronize. Mismatches must stay
/// local — a single mis-highlighted word is acceptable, derailing the rest of
/// the sentence is not.
fn align_word_timings(utterance: &Utterance, words: &[WordTiming]) -> Vec<TimedWord> {
    /// How far ahead either side is searched to resynchronize. Covers the
    /// common expansions (e.g. "123" spoken as three words) without letting
    /// a repeated word later in the sentence steal the match.
    const LOOKAHEAD: usize = 3;

    fn normalized(word: &str) -> String {
        word.chars()
            .filter(|character| character.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }

    // Tokens the provider reports but that carry no speakable content
    // (whitespace, bare punctuation) are dropped up front; their time spans
    // then belong to the previous word, which keeps it lit through pauses.
    let timings: Vec<(Duration, String)> = words
        .iter()
        .filter_map(|word| {
            // Rejects negative, NaN, and overflowing values in one place.
            // Timestamps come from the network, and a corrupt or hostile
            // value must drop the word, not panic mid entity-update.
            let start = Duration::try_from_secs_f32(word.start_secs).ok()?;
            Some((start, normalized(&word.text)))
        })
        .filter(|(_, normalized)| !normalized.is_empty())
        .collect();

    let tokens: Vec<(Range<usize>, String)> = utterance
        .spoken_text
        .split_whitespace()
        .map(|token| {
            let offset = token.as_ptr() as usize - utterance.spoken_text.as_ptr() as usize;
            (offset..offset + token.len(), normalized(token))
        })
        .filter(|(_, normalized)| !normalized.is_empty())
        .collect();

    let source_range_for_token = |token_index: usize| -> Option<Range<usize>> {
        let (spoken_range, _) = tokens.get(token_index)?;
        utterance.source_range_for_spoken(spoken_range.clone())
    };
    let matches = |timing_index: usize, token_index: usize| -> bool {
        match (timings.get(timing_index), tokens.get(token_index)) {
            (Some((_, timing)), Some((_, token))) => timing == token,
            _ => false,
        }
    };

    let mut aligned = Vec::with_capacity(timings.len());
    let mut timing_index = 0;
    let mut token_index = 0;
    while timing_index < timings.len() {
        let start = timings[timing_index].0;
        if token_index >= tokens.len() {
            aligned.push(TimedWord {
                start,
                source_range: None,
            });
            timing_index += 1;
            continue;
        }
        if matches(timing_index, token_index) {
            aligned.push(TimedWord {
                start,
                source_range: source_range_for_token(token_index),
            });
            timing_index += 1;
            token_index += 1;
            continue;
        }
        // Some tokens were not spoken: this timing matches a token ahead.
        if let Some(skip) = (1..=LOOKAHEAD).find(|skip| matches(timing_index, token_index + skip)) {
            token_index += skip;
            continue;
        }
        // Unmatchable extra timings: a later timing matches this token.
        if let Some(skip) = (1..=LOOKAHEAD).find(|skip| matches(timing_index + skip, token_index)) {
            for extra in 0..skip {
                aligned.push(TimedWord {
                    start: timings[timing_index + extra].0,
                    source_range: None,
                });
            }
            timing_index += skip;
            continue;
        }
        // Expansion: this token was spoken as several words (e.g. a number).
        // All of them highlight the token, and matching resumes right after
        // it. With a lookahead of one this doubles as the plain
        // one-for-one-divergence case ("2" spoken as "two").
        if let Some(consumed) =
            (1..=LOOKAHEAD).find(|consumed| matches(timing_index + consumed, token_index + 1))
        {
            let source_range = source_range_for_token(token_index);
            for expanded in 0..consumed {
                aligned.push(TimedWord {
                    start: timings[timing_index + expanded].0,
                    source_range: source_range.clone(),
                });
            }
            timing_index += consumed;
            token_index += 1;
            continue;
        }
        // Both sides diverge with no resynchronization point in sight: pair
        // them up one-for-one and keep going.
        aligned.push(TimedWord {
            start,
            source_range: source_range_for_token(token_index),
        });
        timing_index += 1;
        token_index += 1;
    }

    aligned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FakeTts;
    use crate::sink::FakeSink;
    use gpui::{AppContext as _, Entity, TestAppContext};

    fn utterance(text: &str, start: usize) -> Utterance {
        Utterance {
            source_range: start..start + text.len(),
            spoken_text: text.to_string(),
            // Identity mapping: spoken byte N came from source byte start+N.
            spoken_origins: (start..start + text.len()).collect(),
        }
    }

    fn setup(cx: &mut TestAppContext) -> (Entity<Player>, FakeTts, FakeSink) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let player = cx.new({
            let provider = provider.clone();
            let sink = sink.clone();
            |cx| Player::new(Arc::new(provider), Box::new(sink), None, cx)
        });
        (player, provider, sink)
    }

    #[gpui::test]
    async fn recovering_a_silent_sink_respeaks_the_current_utterance(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let replacement = FakeSink::new();
        // Stands in for the output device having moved: until it is armed, the
        // real recovery reports that the sink in use is still the right one.
        let device_moved = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let player = cx.new({
            let provider = provider.clone();
            let replacement = replacement.clone();
            let device_moved = device_moved.clone();
            |cx| {
                let recover: SinkRecovery = Arc::new(move |_cx, _stalled| {
                    if !device_moved.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        return None;
                    }
                    Some(Box::new(replacement.clone()) as Box<dyn AudioSink>)
                });
                Player::new(Arc::new(provider), Box::new(sink), Some(recover), cx)
            }
        });

        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("One.", 0), utterance("Two.", 5)], cx);
        });
        cx.run_until_parked();
        assert_eq!(provider.spoken(), vec!["One.", "Two."]);
        assert_eq!(replacement.queued_chunks(), 0);

        device_moved.store(true, std::sync::atomic::Ordering::Relaxed);
        player.update(cx, |player, cx| player.poll_position(cx));
        cx.run_until_parked();

        assert_eq!(
            provider.spoken(),
            vec!["One.", "Two.", "One.", "Two."],
            "the utterance that was sounding should be spoken again, on the new device"
        );
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(0)
        );
        assert!(
            replacement.queued_chunks() > 0,
            "the replacement sink should be the one playing"
        );
    }

    /// A wedged output stream is the shape of the bug where audio stops for the
    /// rest of the session: samples are accepted, nothing is heard, and cpal
    /// reports no error.
    #[gpui::test]
    async fn a_stalled_output_stream_is_reopened(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let replacement = FakeSink::new();
        let forced = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let player = cx.new({
            let provider = provider.clone();
            let replacement = replacement.clone();
            let forced = forced.clone();
            |cx| {
                let recover: SinkRecovery = Arc::new(move |_cx, stalled| {
                    // The device never moves in this test, so only the stall
                    // can produce a replacement.
                    if !stalled {
                        return None;
                    }
                    forced.store(true, std::sync::atomic::Ordering::Relaxed);
                    Some(Box::new(replacement.clone()) as Box<dyn AudioSink>)
                });
                Player::new(Arc::new(provider), Box::new(sink), Some(recover), cx)
            }
        });

        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("One.", 0), utterance("Two.", 5)], cx);
        });
        cx.run_until_parked();
        assert_eq!(provider.spoken(), vec!["One.", "Two."]);

        // Audio is queued but the sink never advances: the stream is not
        // sounding. Nothing should happen until the stall window elapses.
        player.update(cx, |player, cx| player.poll_position(cx));
        cx.run_until_parked();
        assert!(!forced.load(std::sync::atomic::Ordering::Relaxed));

        cx.executor().advance_clock(STALL_TIMEOUT);
        player.update(cx, |player, cx| player.poll_position(cx));
        cx.run_until_parked();

        assert!(
            forced.load(std::sync::atomic::Ordering::Relaxed),
            "a stream that stops sounding should be reopened rather than left silent"
        );
        assert!(replacement.queued_chunks() > 0);
    }

    #[gpui::test]
    async fn synthesizes_in_queue_order(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("First.", 0), utterance("Second.", 7)], cx);
        });
        cx.run_until_parked();
        assert_eq!(provider.spoken(), vec!["First.", "Second."]);
    }

    #[gpui::test]
    async fn reports_the_speaking_index(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("One.", 0), utterance("Two.", 5)], cx);
        });
        cx.run_until_parked();
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(0)
        );

        sink.finish_one();
        player.update(cx, |player, cx| player.poll_position(cx));
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(1)
        );
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

        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(2)
        );
        assert_eq!(
            sink.queued(),
            1,
            "seek must discard everything before the target"
        );
        assert_eq!(
            provider.spoken().last().map(String::as_str),
            Some("C."),
            "seek must synthesize the target utterance"
        );
    }

    #[gpui::test]
    async fn seek_before_synthesis_starts_cancels_the_stale_target(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("A.", 0), utterance("B.", 3), utterance("C.", 6)],
                cx,
            );
            // Seek away immediately, before `cx.run_until_parked()` ever lets
            // the synthesis task spawned by `set_utterances` run. If `seek_to`
            // does not cancel it, it later appends "A."'s audio into the
            // post-seek queue and rewinds `next_to_synthesize`, snapping
            // playback back to before the seek.
            player.seek_to(2, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(2)
        );
        assert_eq!(
            sink.queued(),
            1,
            "the canceled pre-seek synthesis must not land in the post-seek queue"
        );
        assert_eq!(
            provider.spoken(),
            vec!["C."],
            "the canceled synthesis must never even reach the provider"
        );
    }

    #[gpui::test]
    async fn synthesis_keeps_pace_with_a_queue_longer_than_the_prefetch(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        let texts = ["One.", "Two.", "Three.", "Four.", "Five."];
        let utterances: Vec<Utterance> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| utterance(text, index * 10))
            .collect();
        player.update(cx, |player, cx| {
            player.set_utterances(utterances, cx);
        });
        cx.run_until_parked();

        // Only the prefetch window (2) is synthesized up front; draining the
        // sink one utterance at a time must keep pulling more synthesis in
        // behind it, all the way to the end.
        for _ in 0..texts.len() {
            assert!(
                player
                    .read_with(cx, |player, _| player.speaking_index())
                    .is_some(),
                "must not report finished before the last utterance has drained"
            );
            sink.finish_one();
            player.update(cx, |player, cx| player.poll_position(cx));
            cx.run_until_parked();
        }

        assert_eq!(
            provider.spoken(),
            texts.to_vec(),
            "every utterance must eventually be synthesized, not just the initial prefetch"
        );
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            None,
            "finished only once the last utterance has drained"
        );
    }

    #[gpui::test]
    async fn a_synthesis_failure_does_not_stall_the_queue(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        provider.fail_next();
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("Doomed.", 0), utterance("Survivor.", 8)], cx);
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
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            None
        );
    }

    #[gpui::test]
    async fn reset_drops_the_queue_and_restarts_synthesis_from_the_top(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("A.", 0), utterance("B.", 3), utterance("C.", 6)],
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            sink.queued(),
            2,
            "prefetch should have queued the first two"
        );

        player.update(cx, |player, cx| player.reset(cx));
        assert_eq!(
            sink.queued(),
            0,
            "reset must drop whatever was already queued, unlike seek_to which \
             only clears the sink when the target index is in range"
        );

        cx.run_until_parked();
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(0),
            "reset must restart synthesis from the first utterance"
        );
        assert_eq!(
            provider.spoken(),
            vec!["A.", "B.", "A.", "B."],
            "reset must resynthesize from the top rather than resume mid-queue"
        );
    }

    #[gpui::test]
    async fn reset_clears_the_queue_even_with_nothing_left_to_synthesize(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("A.", 0)], cx);
        });
        cx.run_until_parked();
        assert_eq!(sink.queued(), 1);

        // Unlike `seek_to(0, ..)`, which early-returns when the target index
        // is out of range and so cannot be used to discard stale audio when
        // there is nothing new to speak yet, `reset` must still clear the
        // sink even though the new utterance list is empty.
        player.update(cx, |player, cx| {
            player.set_utterances(vec![], cx);
            player.reset(cx);
        });
        assert_eq!(
            sink.queued(),
            0,
            "reset must drop stale audio unconditionally"
        );
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            None
        );
    }

    #[gpui::test]
    async fn appending_utterances_does_not_resynthesize_earlier_ones(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("First.", 0)], cx);
        });
        cx.run_until_parked();

        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("First.", 0), utterance("Second.", 7)], cx);
        });
        cx.run_until_parked();

        assert_eq!(
            provider.spoken(),
            vec!["First.", "Second."],
            "streaming appends must not re-speak what was already queued"
        );
    }

    /// The point of streaming: an utterance arrives as several buffers, but the
    /// player still reasons about it as one. If the sink counted chunks
    /// instead, `speaking_index` would run ahead of the audio and the highlight
    /// would jump to the wrong sentence.
    #[gpui::test]
    async fn a_streamed_utterance_is_still_one_queue_slot(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        provider.stream_in_chunks(4);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("Alpha beta gamma delta.", 0)], cx);
        });
        cx.run_until_parked();

        assert_eq!(
            sink.queued_chunks(),
            4,
            "the provider's chunks must reach the sink separately, not be buffered into one"
        );
        assert_eq!(
            sink.queued(),
            1,
            "four chunks of one utterance are one queued utterance"
        );
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(0),
        );

        sink.finish_one();
        player.update(cx, |player, cx| player.poll_position(cx));
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            None,
            "finishing the utterance finishes all of its chunks"
        );
    }

    /// The latency win itself: audio must be queued and playable while the
    /// provider is still producing the rest of the utterance.
    #[gpui::test]
    async fn playback_can_start_before_the_utterance_finishes_streaming(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        provider.stream_in_chunks(3);
        provider.hold();
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("One two three four.", 0)], cx);
        });
        cx.run_until_parked();
        assert_eq!(sink.queued_chunks(), 0, "nothing escapes a held synthesis");

        provider.release_all();
        cx.run_until_parked();
        assert!(
            sink.queued_chunks() > 1,
            "the utterance must reach the sink in pieces, got {}",
            sink.queued_chunks()
        );
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(0),
            "the first chunk is enough to start speaking"
        );
    }

    /// Word timings are relative to the whole utterance, but the underlying
    /// player's clock restarts on every appended chunk. The sink has to carry
    /// the finished chunks forward, or the highlight snaps back to the first
    /// word at every chunk boundary.
    ///
    /// The magnitude of that offset is checked in `sink`'s own tests, where
    /// chunk durations can be stated outright; `FakeTts` emits one sample per
    /// character, so its chunks are sub-millisecond and could not move a
    /// highlight spaced 100ms apart.
    #[gpui::test]
    async fn position_carries_across_a_chunk_boundary(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        provider.emit_word_timings();
        provider.stream_in_chunks(2);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("Alpha beta gamma.", 0)], cx);
        });
        cx.run_until_parked();
        assert_eq!(sink.queued_chunks(), 2);

        sink.set_position(Duration::from_millis(50));
        assert_eq!(
            player.read_with(cx, |player, _| player.current_word_source_range()),
            Some(0..5),
            "50ms into the first chunk, 'Alpha' is sounding"
        );

        // The first chunk finishes; the underlying clock restarts at zero.
        sink.finish_chunk();
        assert_eq!(
            sink.queued(),
            1,
            "finishing one chunk must not retire the whole utterance"
        );
        assert!(
            sink.position() > Duration::ZERO,
            "the elapsed chunk must still count toward the utterance's position, \
             otherwise the highlight restarts at every boundary"
        );
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(0),
            "the utterance is still the one speaking after its first chunk ends"
        );
    }

    /// A provider that dies partway through an utterance must not strand the
    /// queue: the chunks that already landed keep their place and the next
    /// utterance still gets synthesized.
    #[gpui::test]
    async fn a_failure_partway_through_does_not_stall_the_queue(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        provider.stream_in_chunks(3);
        provider.fail_next();
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("Doomed.", 0), utterance("Survivor.", 8)], cx);
        });
        cx.run_until_parked();

        assert_eq!(
            provider.spoken(),
            vec!["Survivor."],
            "the failed utterance is skipped and the queue keeps moving"
        );
    }

    fn timing(text: &str, start_secs: f32) -> WordTiming {
        WordTiming {
            text: text.to_string(),
            start_secs,
            end_secs: start_secs + 0.1,
        }
    }

    /// Runs the aligner and maps each timing to the source text it would
    /// highlight, using the identity-mapped test utterance.
    fn aligned_words(spoken: &str, timings: &[WordTiming]) -> Vec<Option<String>> {
        let utterance = utterance(spoken, 0);
        align_word_timings(&utterance, timings)
            .into_iter()
            .map(|word| word.source_range.map(|range| spoken[range].to_string()))
            .collect()
    }

    #[test]
    fn alignment_matches_identical_words() {
        assert_eq!(
            aligned_words(
                "First one here.",
                &[
                    timing("First", 0.0),
                    timing("one", 0.2),
                    timing("here.", 0.4)
                ],
            ),
            vec![
                Some("First".to_string()),
                Some("one".to_string()),
                Some("here.".to_string()),
            ]
        );
    }

    #[test]
    fn alignment_ignores_case_and_punctuation_divergence() {
        assert_eq!(
            aligned_words(
                "Hello, World.",
                &[timing("hello", 0.0), timing("WORLD", 0.3)],
            ),
            vec![Some("Hello,".to_string()), Some("World.".to_string())]
        );
    }

    #[test]
    fn alignment_drops_whitespace_and_punctuation_timings() {
        // Inworld emits whitespace and punctuation as their own tokens with
        // real durations; they must not produce highlights of their own.
        assert_eq!(
            aligned_words(
                "Hello world.",
                &[
                    timing("Hello", 0.0),
                    timing(" ", 0.31),
                    timing("world", 0.31),
                    timing(".", 0.73),
                    timing("", 1.0),
                ],
            ),
            vec![Some("Hello".to_string()), Some("world.".to_string())]
        );
    }

    #[test]
    fn alignment_pairs_a_normalized_number_with_its_digits() {
        assert_eq!(
            aligned_words(
                "Buy 2 apples now.",
                &[
                    timing("Buy", 0.0),
                    timing("two", 0.2),
                    timing("apples", 0.4),
                    timing("now.", 0.6),
                ],
            ),
            vec![
                Some("Buy".to_string()),
                Some("2".to_string()),
                Some("apples".to_string()),
                Some("now.".to_string()),
            ]
        );
    }

    #[test]
    fn alignment_spreads_a_number_expansion_across_its_source_token() {
        assert_eq!(
            aligned_words(
                "It costs 123 dollars.",
                &[
                    timing("It", 0.0),
                    timing("costs", 0.2),
                    timing("one", 0.4),
                    timing("twenty", 0.6),
                    timing("three", 0.8),
                    timing("dollars.", 1.0),
                ],
            ),
            vec![
                Some("It".to_string()),
                Some("costs".to_string()),
                Some("123".to_string()),
                Some("123".to_string()),
                Some("123".to_string()),
                Some("dollars.".to_string()),
            ]
        );
    }

    #[test]
    fn alignment_survives_a_mismatch_mid_sentence() {
        assert_eq!(
            aligned_words(
                "foo bar baz",
                &[
                    timing("foo", 0.0),
                    timing("blorp", 0.2),
                    timing("bar", 0.4),
                    timing("baz", 0.6),
                ],
            ),
            vec![
                Some("foo".to_string()),
                None,
                Some("bar".to_string()),
                Some("baz".to_string()),
            ],
            "an unmatchable word must not derail the words after it"
        );
    }

    #[test]
    fn alignment_leaves_trailing_unmatched_timings_unhighlighted() {
        assert_eq!(
            aligned_words("foo", &[timing("foo", 0.0), timing("extra", 0.2)]),
            vec![Some("foo".to_string()), None]
        );
    }

    #[test]
    fn alignment_drops_timings_with_untrustworthy_start_times() {
        // Timestamps come off the network. Values `Duration` cannot hold —
        // overflowing, NaN, negative — must drop the word, not panic while
        // the synthesized audio is being accepted.
        assert_eq!(
            aligned_words(
                "foo bar",
                &[
                    timing("foo", 0.0),
                    timing("huge", 1e30),
                    timing("nan", f32::NAN),
                    timing("negative", -1.0),
                    timing("bar", 0.2),
                ],
            ),
            vec![Some("foo".to_string()), Some("bar".to_string())]
        );
    }

    #[gpui::test]
    async fn position_selects_the_current_word(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        provider.emit_word_timings();
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("Alpha beta gamma.", 0)], cx);
        });
        cx.run_until_parked();

        // FakeTts words start at 0.0, 0.1, 0.2 seconds.
        sink.set_position(Duration::from_millis(50));
        assert_eq!(
            player.read_with(cx, |player, _| player.current_word_source_range()),
            Some(0..5),
            "50ms into the audio, 'Alpha' is sounding"
        );

        sink.set_position(Duration::from_millis(150));
        assert_eq!(
            player.read_with(cx, |player, _| player.current_word_source_range()),
            Some(6..10),
            "'beta' starts at 100ms"
        );

        sink.set_position(Duration::from_millis(950));
        assert_eq!(
            player.read_with(cx, |player, _| player.current_word_source_range()),
            Some(11..17),
            "the last word stays lit through trailing silence"
        );
    }

    #[gpui::test]
    async fn position_resets_across_utterance_boundaries(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        provider.emit_word_timings();
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("One two.", 0), utterance("Three four.", 9)],
                cx,
            );
        });
        cx.run_until_parked();

        sink.set_position(Duration::from_millis(150));
        assert_eq!(
            player.read_with(cx, |player, _| player.current_word_source_range()),
            Some(4..8),
            "150ms into the first utterance, 'two.' is sounding"
        );

        // The first utterance finishes; the sink's clock restarts for the
        // next source, exactly as rodio's does.
        sink.finish_one();
        player.update(cx, |player, cx| player.poll_position(cx));
        assert_eq!(
            player.read_with(cx, |player, _| player.current_word_source_range()),
            Some(9..14),
            "at position zero of the second utterance, 'Three' is sounding"
        );
    }

    #[gpui::test]
    async fn no_timings_means_no_word_range(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("Alpha beta.", 0)], cx);
        });
        cx.run_until_parked();

        sink.set_position(Duration::from_millis(150));
        assert_eq!(
            player.read_with(cx, |player, _| player.current_word_source_range()),
            None,
            "a provider without timings degrades to sentence-only highlighting"
        );
    }

    #[gpui::test]
    async fn word_position_accounts_for_playback_speed(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        provider.emit_word_timings();
        player.update(cx, |player, cx| {
            player.set_speed(2.0);
            player.set_utterances(vec![utterance("One two three.", 0)], cx);
        });
        cx.run_until_parked();

        // rodio's position runs on the sped-up output clock: 125ms of output
        // at 2x covers 250ms of the recording, whose word there is 'three.'.
        sink.set_position(Duration::from_millis(125));
        assert_eq!(
            player.read_with(cx, |player, _| player.current_word_source_range()),
            Some(8..14)
        );
    }

    #[gpui::test]
    async fn speed_is_forwarded_to_the_sink(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, _cx| player.set_speed(1.25));
        assert_eq!(sink.speed(), 1.25);
    }

    #[gpui::test]
    async fn word_position_survives_an_absurd_speaking_rate(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        provider.emit_word_timings();
        player.update(cx, |player, cx| {
            // `speaking_rate` arrives unvalidated from user settings; a huge
            // value must clamp, not panic in the per-tick Duration math.
            player.set_speed(1e30);
            player.set_utterances(vec![utterance("One two.", 0)], cx);
        });
        cx.run_until_parked();

        sink.set_position(Duration::from_millis(50));
        assert_eq!(
            player.read_with(cx, |player, _| player.current_word_source_range()),
            Some(4..8),
            "the clamped clock lands past every word start, lighting the last word"
        );
        assert_eq!(
            sink.speed(),
            10.0,
            "the sink must get the same clamped value the highlight clock uses"
        );

        player.update(cx, |player, _cx| player.set_speed(f32::NAN));
        assert_eq!(sink.speed(), 1.0, "non-finite input falls back to 1.0");

        player.update(cx, |player, _cx| player.set_speed(-5.0));
        assert_eq!(sink.speed(), 0.0, "negative input clamps to a standstill");
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
        assert_eq!(
            player.read_with(cx, |player, _| player.speaking_index()),
            Some(0)
        );
    }
}
