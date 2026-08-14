use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, SharedString};
use std::sync::{Arc, Mutex};

/// Raw uncompressed audio. Interleaved if `channels > 1`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Word-level timing for karaoke highlighting, in seconds from the start
    /// of this utterance's audio. Empty when the provider has no timing info;
    /// everything downstream degrades to sentence-level highlighting.
    pub words: Vec<WordTiming>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WordTiming {
    pub text: String,
    pub start_secs: f32,
    pub end_secs: f32,
}

/// One seam so no TTS vendor is welded into the editor.
pub trait TtsProvider: Send + Sync + 'static {
    /// Streams one utterance's audio in playback order.
    ///
    /// Chunks are yielded as the provider produces them so playback can start
    /// before the whole utterance is synthesized — that head start is the
    /// difference between speaking immediately and waiting out a full
    /// round-trip. Providers with nothing to stream may yield a single chunk.
    ///
    /// `words` on each chunk carries the utterance's word timings *known so
    /// far*, relative to the start of the utterance rather than the chunk, so a
    /// later chunk supersedes an earlier one's list. Dropping the receiver
    /// cancels the work.
    fn synthesize(&self, text: String, cx: &App) -> mpsc::UnboundedReceiver<Result<Pcm>>;
}

/// Collects a whole utterance from a streaming provider, concatenating its
/// chunks. Used where the caller genuinely needs the complete audio rather than
/// a head start.
pub async fn collect_utterance(mut chunks: mpsc::UnboundedReceiver<Result<Pcm>>) -> Result<Pcm> {
    let mut collected: Option<Pcm> = None;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        match &mut collected {
            Some(collected) => {
                collected.samples.extend(chunk.samples);
                // Later chunks carry the cumulative timing list, so the longest
                // one wins rather than concatenating duplicates.
                if chunk.words.len() >= collected.words.len() {
                    collected.words = chunk.words;
                }
            }
            None => collected = Some(chunk),
        }
    }
    collected.ok_or_else(|| anyhow!("the provider produced no audio"))
}

/// A selectable voice, as offered by a provider's catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct TtsVoice {
    /// The identifier passed to synthesis (`read_aloud.voice_id`).
    pub id: SharedString,
    /// What the voice menu shows; falls back to the id when the provider has
    /// no separate display name.
    pub name: SharedString,
}

/// Both providers hand back self-contained WAV chunks: a RIFF/WAVE
/// container header followed by a `data` subchunk holding the raw LINEAR16
/// samples. Decoding the header bytes as PCM produces an audible click at
/// every chunk boundary, so locate and strip the header first. The `data`
/// subchunk is not always at a fixed offset (extra subchunks, e.g. `fact`,
/// can precede it), so this walks the chunk list using each subchunk's
/// declared length rather than assuming a fixed 44-byte header. Chunks that
/// are not RIFF/WAVE (i.e. already-raw PCM) are returned unchanged.
pub(crate) fn strip_wav_header(bytes: &[u8]) -> &[u8] {
    const RIFF: &[u8] = b"RIFF";
    const WAVE: &[u8] = b"WAVE";
    const DATA: &[u8] = b"data";
    const RIFF_HEADER_LEN: usize = 12;
    const SUBCHUNK_HEADER_LEN: usize = 8;

    if bytes.len() < RIFF_HEADER_LEN || &bytes[0..4] != RIFF || &bytes[8..12] != WAVE {
        return bytes;
    }

    let mut offset = RIFF_HEADER_LEN;
    while offset + SUBCHUNK_HEADER_LEN <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let Ok(size_bytes) = <[u8; 4]>::try_from(&bytes[offset + 4..offset + 8]) else {
            break;
        };
        let size = u32::from_le_bytes(size_bytes) as usize;
        let data_start = offset + SUBCHUNK_HEADER_LEN;

        if id == DATA {
            let data_end = data_start.saturating_add(size).min(bytes.len());
            return &bytes[data_start..data_end];
        }

        // Subchunks are padded to an even number of bytes.
        let padded_size = size + (size % 2);
        offset = data_start.saturating_add(padded_size);
    }

    bytes
}

/// LINEAR16 is little-endian signed 16-bit PCM. A trailing odd byte is not a
/// sample and is discarded.
pub(crate) fn decode_linear16(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
        .collect()
}

struct FakeTtsState {
    spoken: Vec<String>,
    fail_next: bool,
    emit_word_timings: bool,
    hold: bool,
    held: Vec<futures::channel::oneshot::Sender<()>>,
    /// How many chunks each utterance is streamed in. One reproduces the
    /// pre-streaming behavior.
    chunks_per_utterance: usize,
}

impl Default for FakeTtsState {
    fn default() -> Self {
        Self {
            spoken: Vec::new(),
            fail_next: false,
            emit_word_timings: false,
            hold: false,
            held: Vec::new(),
            chunks_per_utterance: 1,
        }
    }
}

/// Test double. Produces one silent sample per character so queue ordering
/// and duration are predictable without touching the network.
#[derive(Clone, Default)]
pub struct FakeTts {
    state: Arc<Mutex<FakeTtsState>>,
}

impl FakeTts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spoken(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.spoken.clone())
            .unwrap_or_default()
    }

    pub fn fail_next(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.fail_next = true;
        }
    }

    /// Makes every subsequent synthesis emit one `WordTiming` per
    /// whitespace-separated word, each 100ms long, so position→word math is
    /// predictable in tests.
    pub fn emit_word_timings(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.emit_word_timings = true;
        }
    }

    /// Streams every subsequent utterance across `count` chunks, so tests can
    /// tell the streaming path from the single-buffer one.
    pub fn stream_in_chunks(&self, count: usize) {
        if let Ok(mut state) = self.state.lock() {
            state.chunks_per_utterance = count;
        }
    }

    /// Holds every subsequent synthesis in flight until [`Self::release_all`],
    /// so tests can reproduce the real provider's network latency.
    pub fn hold(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.hold = true;
        }
    }

    /// Completes every synthesis started while holding, and stops holding
    /// new ones.
    pub fn release_all(&self) {
        let held = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.hold = false;
            std::mem::take(&mut state.held)
        };
        for sender in held {
            sender.send(()).ok();
        }
    }

    /// Splits the utterance's audio across `chunks_per_utterance` messages, so
    /// tests can exercise the streaming path. One chunk — the default — is the
    /// non-streaming shape every pre-existing test assumes.
    fn finish_synthesis(&self, text: String) -> Vec<Result<Pcm>> {
        let Ok(mut state) = self.state.lock() else {
            return vec![Err(anyhow!("FakeTts state poisoned"))];
        };
        state.spoken.push(text.clone());
        let words: Vec<WordTiming> = if state.emit_word_timings {
            text.split_whitespace()
                .enumerate()
                .map(|(index, word)| WordTiming {
                    text: word.to_string(),
                    start_secs: index as f32 * FAKE_WORD_DURATION_SECS,
                    end_secs: (index + 1) as f32 * FAKE_WORD_DURATION_SECS,
                })
                .collect()
        } else {
            Vec::new()
        };

        let total_samples = text.chars().count();
        let chunk_count = state.chunks_per_utterance.max(1).min(total_samples.max(1));
        let per_chunk = total_samples.div_ceil(chunk_count);

        (0..chunk_count)
            .map(|chunk| {
                let start = chunk * per_chunk;
                let end = ((chunk + 1) * per_chunk).min(total_samples);
                Ok(Pcm {
                    samples: vec![0.0; end.saturating_sub(start)],
                    sample_rate: 22050,
                    channels: 1,
                    // Timings are cumulative for the utterance, as the real
                    // provider's are, so every chunk carries the full list.
                    words: words.clone(),
                })
            })
            .collect()
    }
}

/// One fake word every 100ms, mirroring how Inworld reports timings relative
/// to the start of the utterance's audio.
pub const FAKE_WORD_DURATION_SECS: f32 = 0.1;

impl TtsProvider for FakeTts {
    fn synthesize(&self, text: String, cx: &App) -> mpsc::UnboundedReceiver<Result<Pcm>> {
        let (sender, receiver) = mpsc::unbounded();
        let held = {
            let Ok(mut state) = self.state.lock() else {
                sender
                    .unbounded_send(Err(anyhow!("FakeTts state poisoned")))
                    .ok();
                return receiver;
            };
            if std::mem::take(&mut state.fail_next) {
                sender
                    .unbounded_send(Err(anyhow!("FakeTts was told to fail")))
                    .ok();
                return receiver;
            }
            if state.hold {
                let (release_tx, release_rx) = futures::channel::oneshot::channel();
                state.held.push(release_tx);
                Some(release_rx)
            } else {
                None
            }
        };

        let this = self.clone();
        cx.background_spawn(async move {
            if let Some(held) = held
                && held.await.is_err()
            {
                sender
                    .unbounded_send(Err(anyhow!("FakeTts dropped a held synthesis")))
                    .ok();
                return;
            }
            for chunk in this.finish_synthesis(text) {
                // A closed receiver means the player cancelled; stop producing.
                if sender.unbounded_send(chunk).is_err() {
                    return;
                }
            }
        })
        .detach();

        receiver
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn fake_provider_returns_one_sample_per_character(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let pcm = collect_utterance(cx.update(|cx| provider.synthesize("hello".to_string(), cx)))
            .await
            .unwrap();
        assert_eq!(pcm.samples.len(), 5);
        assert_eq!(pcm.sample_rate, 22050);
        assert_eq!(pcm.channels, 1);
    }

    #[gpui::test]
    async fn fake_provider_records_what_it_was_asked_to_say(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        collect_utterance(cx.update(|cx| provider.synthesize("first".to_string(), cx)))
            .await
            .unwrap();
        collect_utterance(cx.update(|cx| provider.synthesize("second".to_string(), cx)))
            .await
            .unwrap();
        assert_eq!(provider.spoken(), vec!["first", "second"]);
    }

    #[gpui::test]
    async fn fake_provider_emits_word_timings_only_when_asked(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let pcm =
            collect_utterance(cx.update(|cx| provider.synthesize("no timings".to_string(), cx)))
                .await
                .unwrap();
        assert!(pcm.words.is_empty());

        provider.emit_word_timings();
        let pcm =
            collect_utterance(cx.update(|cx| provider.synthesize("two words".to_string(), cx)))
                .await
                .unwrap();
        assert_eq!(
            pcm.words,
            vec![
                WordTiming {
                    text: "two".to_string(),
                    start_secs: 0.0,
                    end_secs: 0.1,
                },
                WordTiming {
                    text: "words".to_string(),
                    start_secs: 0.1,
                    end_secs: 0.2,
                },
            ]
        );
    }

    #[gpui::test]
    async fn fake_provider_can_be_told_to_fail(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        provider.fail_next();
        let result =
            collect_utterance(cx.update(|cx| provider.synthesize("doomed".to_string(), cx))).await;
        assert!(result.is_err());
    }
}
