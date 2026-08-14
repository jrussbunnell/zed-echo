use crate::provider::{Pcm, TtsProvider, TtsVoice, WordTiming, decode_linear16, strip_wav_header};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use futures::channel::mpsc;
use futures::io::BufReader;
use futures::{AsyncBufReadExt as _, AsyncReadExt as _, StreamExt as _};
use gpui::{App, AppContext as _, SharedString, Task};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use std::sync::{Arc, Mutex};

pub const INWORLD_API_URL: &str = "https://api.inworld.ai/tts/v1/voice:stream";
pub const INWORLD_VOICES_URL: &str = "https://api.inworld.ai/tts/v1/voices";
pub const INWORLD_CREDENTIALS_URL: &str = "https://api.inworld.ai";
const INWORLD_API_KEY_VAR: &str = "INWORLD_API_KEY";
const SAMPLE_RATE: u32 = 22050;
const RATE_LIMIT_MAX_RETRIES: u32 = 2;
const RATE_LIMIT_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

pub struct InworldTts {
    client: Arc<dyn HttpClient>,
    api_key: String,
    /// Read at each synthesize call rather than captured at construction, so
    /// a settings change speaks in the new voice from the next request on —
    /// already-synthesized audio keeps the voice it was made with.
    voice: Mutex<VoiceSelection>,
}

struct VoiceSelection {
    voice_id: String,
    model_id: String,
}

impl InworldTts {
    pub fn new(
        client: Arc<dyn HttpClient>,
        api_key: String,
        voice_id: String,
        model_id: String,
    ) -> Self {
        Self {
            client,
            api_key,
            voice: Mutex::new(VoiceSelection { voice_id, model_id }),
        }
    }

    /// Applies to the next synthesis request. Poisoning is unreachable in
    /// practice (nothing panics while holding the lock), but a poisoned
    /// selection keeps its old voice rather than crashing the reader.
    pub fn set_voice(&self, voice_id: String, model_id: String) {
        match self.voice.lock() {
            Ok(mut voice) => {
                voice.voice_id = voice_id;
                voice.model_id = model_id;
            }
            Err(error) => log::error!("read_aloud: voice selection poisoned: {error}"),
        }
    }
}

impl TtsProvider for InworldTts {
    fn synthesize(&self, text: String, cx: &App) -> mpsc::UnboundedReceiver<Result<Pcm>> {
        let (sender, receiver) = mpsc::unbounded();
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let (voice_id, model_id) = match self.voice.lock() {
            Ok(voice) => (voice.voice_id.clone(), voice.model_id.clone()),
            Err(error) => {
                sender
                    .unbounded_send(Err(anyhow!("voice selection poisoned: {error}")))
                    .ok();
                return receiver;
            }
        };

        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let result = stream_utterance(
                client,
                api_key,
                text,
                voice_id,
                model_id,
                executor,
                sender.clone(),
            )
            .await;
            if let Err(error) = result {
                sender.unbounded_send(Err(error)).ok();
            }
        })
        .detach();

        receiver
    }
}

/// Issues the request and forwards each streamed chunk as it decodes.
///
/// The endpoint is `voice:stream`: the body is JSON-lines, one object per audio
/// chunk, and each chunk is a self-contained WAV. Reading it to the end before
/// decoding — which is what this used to do — meant waiting out synthesis of
/// the entire utterance before a single sample could play. Forwarding per line
/// is the whole point of the streaming endpoint.
#[allow(clippy::too_many_arguments)]
async fn stream_utterance(
    client: Arc<dyn HttpClient>,
    api_key: String,
    text: String,
    voice_id: String,
    model_id: String,
    executor: gpui::BackgroundExecutor,
    sender: mpsc::UnboundedSender<Result<Pcm>>,
) -> Result<()> {
    let body = serde_json::json!({
        "text": text,
        "voiceId": voice_id,
        "modelId": model_id,
        "audioConfig": {
            "audioEncoding": "LINEAR16",
            "sampleRateHertz": SAMPLE_RATE,
        },
        "deliveryMode": "BALANCED",
        "timestampType": "WORD",
    });
    let body = serde_json::to_string(&body)?;

    let mut backoff = RATE_LIMIT_INITIAL_BACKOFF;
    for attempt in 0..=RATE_LIMIT_MAX_RETRIES {
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri(INWORLD_API_URL)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Basic {}", api_key.trim()))
            .body(AsyncBody::from(body.clone()))?;

        let mut response = client.send(request).await?;
        let status = response.status();

        if !status.is_success() {
            // The error body is small and only read on the failure path, so
            // buffering it whole costs nothing.
            let mut error_body = String::new();
            response.body_mut().read_to_string(&mut error_body).await?;

            // Back off on rate limits, but give up quickly rather than
            // holding the queue. A dropped utterance is better than a
            // stalled panel.
            if status == http_client::StatusCode::TOO_MANY_REQUESTS
                && attempt < RATE_LIMIT_MAX_RETRIES
            {
                log::warn!("read_aloud: Inworld rate limited, retrying in {backoff:?}");
                executor.timer(backoff).await;
                backoff *= 2;
                continue;
            }

            return Err(anyhow!(
                "Inworld TTS returned {status}: {}",
                error_body.trim()
            ));
        }

        let mut lines = BufReader::new(response.into_body()).lines();
        // Word timings arrive per chunk but are timed against the whole
        // utterance, so they accumulate and every chunk carries the list so
        // far. The player realigns on each one.
        let mut words = Vec::new();
        let mut sent_any = false;

        while let Some(line) = lines.next().await {
            let line = line?;
            let Some((samples, chunk_words)) = decode_audio_line(&line)? else {
                continue;
            };
            words.extend(chunk_words);
            if samples.is_empty() {
                continue;
            }
            sent_any = true;
            let chunk = Pcm {
                samples,
                sample_rate: SAMPLE_RATE,
                channels: 1,
                words: words.clone(),
            };
            // A closed receiver means the player cancelled this utterance —
            // stop pulling the body rather than synthesizing into the void.
            if sender.unbounded_send(Ok(chunk)).is_err() {
                return Ok(());
            }
        }

        if !sent_any {
            return Err(anyhow!("Inworld response contained no audio content"));
        }
        return Ok(());
    }

    Err(anyhow!("Inworld TTS exhausted rate-limit retries"))
}

/// Fetches the provider's voice catalog. English voices only, sorted by
/// display name: the full catalog is 250+ voices across a dozen languages,
/// and a context menu of Russian narrators is no help for English prose.
pub fn fetch_voices(
    client: Arc<dyn HttpClient>,
    api_key: String,
    cx: &App,
) -> Task<Result<Vec<TtsVoice>>> {
    cx.background_spawn(async move {
        let request = HttpRequest::builder()
            .method(Method::GET)
            .uri(INWORLD_VOICES_URL)
            .header("Authorization", format!("Basic {}", api_key.trim()))
            .body(AsyncBody::empty())?;
        let mut response = client.send(request).await?;
        let status = response.status();
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        if !status.is_success() {
            return Err(anyhow!(
                "Inworld voice list returned {status}: {}",
                body.trim()
            ));
        }
        parse_voice_list(&body)
    })
}

/// Response shape verified against the live API (2026-08):
/// `{"voices": [{"voiceId", "displayName", "languages": ["en", ...], ...}]}`.
/// An empty result is an error rather than an empty menu, so callers fall
/// back to the curated list.
fn parse_voice_list(body: &str) -> Result<Vec<TtsVoice>> {
    let value: serde_json::Value =
        serde_json::from_str(body).context("Inworld voice list was not valid JSON")?;
    let voices = value
        .get("voices")
        .and_then(|voices| voices.as_array())
        .context("Inworld voice list had no `voices` array")?;
    let mut parsed: Vec<TtsVoice> = voices
        .iter()
        .filter_map(|voice| {
            let id = voice.get("voiceId")?.as_str()?;
            let speaks_english = voice
                .get("languages")
                .and_then(|languages| languages.as_array())
                .is_some_and(|languages| {
                    languages
                        .iter()
                        .any(|language| language.as_str() == Some("en"))
                });
            if !speaks_english {
                return None;
            }
            let name = voice
                .get("displayName")
                .and_then(|name| name.as_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            Some(TtsVoice {
                id: SharedString::from(id.to_string()),
                name: SharedString::from(name.to_string()),
            })
        })
        .collect();
    if parsed.is_empty() {
        return Err(anyhow!("Inworld voice list contained no English voices"));
    }
    parsed.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(parsed)
}

/// A hand-curated slice of real Inworld voices for when the catalog fetch
/// fails. Every id verified against the live API.
pub fn fallback_voices() -> Vec<TtsVoice> {
    [
        "Ashley", "Clive", "Dennis", "Duncan", "Mark", "Olivia", "Sarah", "Timothy",
    ]
    .into_iter()
    .map(|name| TtsVoice {
        id: name.into(),
        name: name.into(),
    })
    .collect()
}

/// Decodes one JSON-lines record into its samples and word timings.
///
/// `Ok(None)` covers the lines that carry no audio — blanks, keep-alives, and
/// anything unparseable. Those are skipped rather than failing the utterance,
/// matching how the buffered decoder behaved. Only malformed base64, which
/// means the stream itself is corrupt, is an error.
fn decode_audio_line(line: &str) -> Result<Option<(Vec<f32>, Vec<WordTiming>)>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok(None);
    };
    let Some(result) = value.get("result") else {
        return Ok(None);
    };
    let Some(encoded) = result
        .get("audioContent")
        .and_then(|content| content.as_str())
    else {
        return Ok(None);
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("Inworld returned audioContent that is not valid base64")?;
    let samples = decode_linear16(strip_wav_header(&decoded));
    let mut words = Vec::new();
    collect_word_alignment(result, &mut words);
    Ok(Some((samples, words)))
}

/// Word timestamps arrive as three parallel arrays under
/// `timestampInfo.wordAlignment`. Verified against the live API: the times of
/// later chunks continue from where the previous chunk ended (they are
/// relative to the whole utterance, not restarted per chunk), so entries are
/// concatenated as-is. The token list includes whitespace-only, punctuation,
/// and empty tokens; those are kept here and filtered by the aligner, which
/// treats anything without an alphanumeric character as unspoken.
fn collect_word_alignment(result: &serde_json::Value, words: &mut Vec<WordTiming>) {
    let Some(alignment) = result
        .get("timestampInfo")
        .and_then(|info| info.get("wordAlignment"))
    else {
        return;
    };
    let (Some(texts), Some(starts), Some(ends)) = (
        alignment.get("words").and_then(|value| value.as_array()),
        alignment
            .get("wordStartTimeSeconds")
            .and_then(|value| value.as_array()),
        alignment
            .get("wordEndTimeSeconds")
            .and_then(|value| value.as_array()),
    ) else {
        return;
    };
    for ((text, start), end) in texts.iter().zip(starts).zip(ends) {
        let (Some(text), Some(start), Some(end)) = (text.as_str(), start.as_f64(), end.as_f64())
        else {
            continue;
        };
        words.push(WordTiming {
            text: text.to_string(),
            start_secs: start as f32,
            end_secs: end as f32,
        });
    }
}

/// Environment variable first, then the keychain. Never `settings.json`.
pub fn resolve_api_key(cx: &App) -> Task<Result<String>> {
    if let Ok(key) = std::env::var(INWORLD_API_KEY_VAR) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Task::ready(Ok(key));
        }
    }

    let credentials = cx.read_credentials(INWORLD_CREDENTIALS_URL);
    cx.background_spawn(async move {
        let (_username, secret) = credentials.await?.context(
            "No Inworld API key found. Set INWORLD_API_KEY or store one in the keychain.",
        )?;
        Ok(String::from_utf8(secret)?.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::collect_utterance;

    /// Test-only aggregation of a whole JSON-lines body, composing
    /// `decode_audio_line` the way the streaming path does. Production
    /// forwards each line as its own chunk instead of concatenating.
    /// Walks the streamed JSON-lines body, concatenating every `result.audioContent`
    /// chunk and every `result.timestampInfo.wordAlignment` entry. Malformed lines
    /// are skipped rather than failing the whole utterance, and missing timestamp
    /// info degrades to an empty word list rather than an error.
    fn collect_audio_content(body: &str) -> Result<(Vec<f32>, Vec<WordTiming>)> {
        let mut samples = Vec::new();
        let mut words = Vec::new();
        let mut found_any = false;

        for line in body.lines() {
            let Some((chunk_samples, chunk_words)) = decode_audio_line(line)? else {
                continue;
            };
            samples.extend(chunk_samples);
            words.extend(chunk_words);
            found_any = true;
        }

        if !found_any {
            return Err(anyhow!("Inworld response contained no audio content"));
        }
        Ok((samples, words))
    }

    use gpui::TestAppContext;

    /// A fake Inworld endpoint that records every request body it is sent
    /// and answers with one valid single-chunk synthesis response.
    fn recording_client() -> (Arc<dyn HttpClient>, Arc<Mutex<Vec<String>>>) {
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let client = http_client::FakeHttpClient::create({
            let bodies = bodies.clone();
            move |mut request| {
                let bodies = bodies.clone();
                async move {
                    let mut body = String::new();
                    request.body_mut().read_to_string(&mut body).await?;
                    bodies.lock().expect("test lock").push(body);
                    Ok(http_client::Response::builder()
                        .status(200)
                        .body(AsyncBody::from(
                            "{\"result\":{\"audioContent\":\"AAA=\"}}\n".to_string(),
                        ))?)
                }
            }
        });
        (client, bodies)
    }

    /// Answers with a body of `chunk_count` JSON-lines records, each a
    /// single-sample WAV, mirroring the shape of the `voice:stream` endpoint.
    fn streaming_client(chunk_count: usize) -> Arc<dyn HttpClient> {
        // A minimal RIFF/WAVE container holding one 16-bit sample, so the
        // decoder's header-stripping runs on the streamed path too.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&2u32.to_le_bytes());
        wav.extend_from_slice(&1234i16.to_le_bytes());
        let encoded = base64::engine::general_purpose::STANDARD.encode(&wav);

        let body: String = (0..chunk_count)
            .map(|chunk| {
                format!(
                    "{{\"result\":{{\"audioContent\":\"{encoded}\",\"timestampInfo\":\
                     {{\"wordAlignment\":{{\"words\":[\"w{chunk}\"],\
                     \"wordStartTimeSeconds\":[{chunk}.0],\
                     \"wordEndTimeSeconds\":[{}.0]}}}}}}}}\n",
                    chunk + 1
                )
            })
            .collect();

        http_client::FakeHttpClient::create(move |_request| {
            let body = body.clone();
            async move {
                Ok(http_client::Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body))?)
            }
        })
    }

    /// The whole point of #2: the endpoint streams JSON-lines, and each line
    /// must reach the player as its own chunk. Buffering the body to the end
    /// before decoding — which is what this used to do — meant waiting out
    /// synthesis of the entire utterance before a single sample could play.
    #[gpui::test]
    async fn each_streamed_line_is_forwarded_as_its_own_chunk(cx: &mut TestAppContext) {
        let tts = InworldTts::new(
            streaming_client(3),
            "key".to_string(),
            "Dennis".to_string(),
            "inworld-tts-2".to_string(),
        );

        let mut chunks = cx.update(|cx| tts.synthesize("Three chunks.".to_string(), cx));
        let mut received = Vec::new();
        while let Some(chunk) = chunks.next().await {
            received.push(chunk.expect("every chunk decodes"));
        }

        assert_eq!(
            received.len(),
            3,
            "one chunk per streamed line, not one buffered utterance"
        );
        for chunk in &received {
            assert_eq!(
                chunk.samples.len(),
                1,
                "the WAV header must be stripped on the streamed path too"
            );
            assert_eq!(chunk.sample_rate, SAMPLE_RATE);
        }
        assert_eq!(
            received
                .iter()
                .map(|chunk| chunk.words.len())
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "word timings are cumulative for the utterance, so each chunk \
             supersedes the last rather than restarting"
        );
    }

    #[gpui::test]
    async fn a_response_with_no_audio_is_an_error(cx: &mut TestAppContext) {
        let client = http_client::FakeHttpClient::create(move |_request| async move {
            Ok(http_client::Response::builder()
                .status(200)
                .body(AsyncBody::from("{\"result\":{}}\n".to_string()))?)
        });
        let tts = InworldTts::new(
            client,
            "key".to_string(),
            "Dennis".to_string(),
            "inworld-tts-2".to_string(),
        );

        let result =
            collect_utterance(cx.update(|cx| tts.synthesize("Silent.".to_string(), cx))).await;
        assert!(
            result.is_err(),
            "an utterance that produced no audio must surface, not play as silence"
        );
    }

    #[gpui::test]
    async fn a_voice_change_applies_to_the_next_synthesis_request(cx: &mut TestAppContext) {
        let (client, bodies) = recording_client();
        let tts = InworldTts::new(
            client,
            "key".to_string(),
            "Dennis".to_string(),
            "inworld-tts-2".to_string(),
        );

        collect_utterance(cx.update(|cx| tts.synthesize("First.".to_string(), cx)))
            .await
            .unwrap();
        tts.set_voice("Clive".to_string(), "inworld-tts-2".to_string());
        collect_utterance(cx.update(|cx| tts.synthesize("Second.".to_string(), cx)))
            .await
            .unwrap();

        let bodies = bodies.lock().expect("test lock");
        let requests: Vec<serde_json::Value> = bodies
            .iter()
            .map(|body| serde_json::from_str(body).expect("request body is JSON"))
            .collect();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["voiceId"], "Dennis");
        assert_eq!(
            requests[1]["voiceId"], "Clive",
            "the voice read at call time, not construction time"
        );
        assert_eq!(requests[1]["modelId"], "inworld-tts-2");
    }

    #[test]
    fn parses_the_voice_list_and_keeps_only_english_voices() {
        // Shape from a live `GET /tts/v1/voices` call (2026-08), trimmed to
        // the fields the parser reads.
        let body = concat!(
            "{\"voices\":[",
            "{\"languages\":[\"ru\"],\"voiceId\":\"Nikolai\",\"displayName\":\"Nikolai\",",
            "\"description\":\"redacted\",\"tags\":[\"deep\"],\"isCustom\":false},",
            "{\"languages\":[\"en\"],\"voiceId\":\"Duncan\",\"displayName\":\"Duncan\",",
            "\"description\":\"redacted\",\"tags\":[],\"isCustom\":false},",
            "{\"languages\":[\"en\"],\"voiceId\":\"Ashley\",\"displayName\":\"\",",
            "\"description\":\"redacted\",\"tags\":[],\"isCustom\":false}",
            "]}"
        );
        let voices = parse_voice_list(body).unwrap();
        assert_eq!(
            voices,
            vec![
                TtsVoice {
                    id: "Ashley".into(),
                    name: "Ashley".into(),
                },
                TtsVoice {
                    id: "Duncan".into(),
                    name: "Duncan".into(),
                },
            ],
            "non-English voices are dropped, an empty display name falls back \
             to the id, and the list is sorted by name"
        );
    }

    #[test]
    fn a_useless_voice_list_is_an_error_not_an_empty_menu() {
        assert!(parse_voice_list("not json").is_err());
        assert!(parse_voice_list("{}").is_err());
        assert!(
            parse_voice_list("{\"voices\":[{\"languages\":[\"ru\"],\"voiceId\":\"Nikolai\"}]}")
                .is_err(),
            "a list with no English voices must trigger the fallback"
        );
    }

    #[test]
    fn fallback_voices_include_the_documented_defaults() {
        let fallback = fallback_voices();
        for expected in ["Dennis", "Clive", "Ashley"] {
            assert!(
                fallback.iter().any(|voice| voice.id.as_ref() == expected),
                "fallback list must include {expected}"
            );
        }
    }

    #[test]
    fn decodes_linear16_to_normalized_floats() {
        // i16 little-endian: 0, 32767, -32768
        let bytes = [0x00, 0x00, 0xff, 0x7f, 0x00, 0x80];
        let samples = decode_linear16(&bytes);
        assert_eq!(samples.len(), 3);
        assert!((samples[0] - 0.0).abs() < 1e-6);
        assert!((samples[1] - 1.0).abs() < 1e-4);
        assert!((samples[2] + 1.0).abs() < 1e-4);
    }

    #[test]
    fn ignores_a_trailing_odd_byte() {
        let samples = decode_linear16(&[0x00, 0x00, 0x01]);
        assert_eq!(samples.len(), 1, "a dangling byte is not half a sample");
    }

    #[test]
    fn extracts_audio_from_streamed_json_lines() {
        // "AAA=" is base64 for two zero bytes -> one zero sample.
        let body = "{\"result\":{\"audioContent\":\"AAA=\"}}\n\
                    {\"result\":{\"audioContent\":\"AAA=\"}}\n";
        let (samples, words) = collect_audio_content(body).unwrap();
        assert_eq!(samples.len(), 2);
        assert!(
            words.is_empty(),
            "no timestampInfo must degrade to an empty word list"
        );
    }

    #[test]
    fn tolerates_blank_and_malformed_lines() {
        let body = "\n{\"result\":{\"audioContent\":\"AAA=\"}}\nnot json\n{}\n";
        let (samples, _) = collect_audio_content(body).unwrap();
        assert_eq!(samples.len(), 1, "one good line still yields its audio");
    }

    #[test]
    fn concatenates_word_alignment_across_chunks() {
        // Shape taken from a live `timestampType: WORD` response (audio
        // content replaced with a single zero sample): each chunk carries
        // parallel `words`/`wordStartTimeSeconds`/`wordEndTimeSeconds`
        // arrays, later chunks continue the earlier chunk's clock, and the
        // token list includes whitespace and punctuation entries.
        let body = concat!(
            "{\"result\":{\"audioContent\":\"AAA=\",\"timestampInfo\":{\"wordAlignment\":{",
            "\"words\":[\"Hello\"],",
            "\"wordStartTimeSeconds\":[0],",
            "\"wordEndTimeSeconds\":[0.31]}}}}\n",
            "{\"result\":{\"audioContent\":\"AAA=\",\"timestampInfo\":{\"wordAlignment\":{",
            "\"words\":[\" \",\"world\",\".\"],",
            "\"wordStartTimeSeconds\":[0.31,0.31,0.73],",
            "\"wordEndTimeSeconds\":[0.31,0.73,1.0]}}}}\n",
        );
        let (samples, words) = collect_audio_content(body).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(
            words,
            vec![
                WordTiming {
                    text: "Hello".to_string(),
                    start_secs: 0.0,
                    end_secs: 0.31,
                },
                WordTiming {
                    text: " ".to_string(),
                    start_secs: 0.31,
                    end_secs: 0.31,
                },
                WordTiming {
                    text: "world".to_string(),
                    start_secs: 0.31,
                    end_secs: 0.73,
                },
                WordTiming {
                    text: ".".to_string(),
                    start_secs: 0.73,
                    end_secs: 1.0,
                },
            ],
            "times must be kept utterance-relative exactly as the API sent them"
        );
    }

    #[test]
    fn tolerates_malformed_word_alignment() {
        // Mismatched array lengths zip down to the shortest; non-string and
        // non-numeric entries are skipped without dropping the audio.
        let body = concat!(
            "{\"result\":{\"audioContent\":\"AAA=\",\"timestampInfo\":{\"wordAlignment\":{",
            "\"words\":[\"one\",2,\"three\"],",
            "\"wordStartTimeSeconds\":[0,0.1],",
            "\"wordEndTimeSeconds\":[0.1,0.2,0.3]}}}}\n",
        );
        let (samples, words) = collect_audio_content(body).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(
            words,
            vec![WordTiming {
                text: "one".to_string(),
                start_secs: 0.0,
                end_secs: 0.1,
            }]
        );
    }

    #[test]
    fn passes_absurd_timestamp_values_through_unsanitized() {
        // A hostile or corrupt response can carry timestamps far beyond what
        // a `Duration` can hold. Parsing keeps them as plain floats — it is
        // the aligner's job to reject them — so this pins down that the
        // parser neither panics on nor silently rewrites such values.
        let body = concat!(
            "{\"result\":{\"audioContent\":\"AAA=\",\"timestampInfo\":{\"wordAlignment\":{",
            "\"words\":[\"huge\"],",
            "\"wordStartTimeSeconds\":[1e30],",
            "\"wordEndTimeSeconds\":[1e30]}}}}\n",
        );
        let (samples, words) = collect_audio_content(body).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].start_secs, 1e30);
    }

    #[test]
    fn errors_when_the_response_contains_no_audio() {
        assert!(collect_audio_content("{}\n").is_err());
    }

    #[test]
    fn word_timings_survive_wav_header_stripping() {
        // A chunk whose audio is a full WAV container and whose alignment is
        // present must yield both the samples and the timings.
        let sample_bytes = [0x00, 0x00, 0xff, 0x7f];
        let wav = wrap_in_wav_header(&sample_bytes);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&wav);
        let body = format!(
            "{{\"result\":{{\"audioContent\":\"{encoded}\",\"timestampInfo\":{{\"wordAlignment\":{{\
             \"words\":[\"hey\"],\"wordStartTimeSeconds\":[0],\"wordEndTimeSeconds\":[0.5]}}}}}}}}\n"
        );
        let (samples, words) = collect_audio_content(&body).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "hey");
    }

    /// Builds a minimal 44-byte canonical WAV header (RIFF + fmt + data)
    /// around the given LINEAR16 sample bytes.
    fn wrap_in_wav_header(sample_bytes: &[u8]) -> Vec<u8> {
        let mut wav = Vec::new();
        let data_len = sample_bytes.len() as u32;
        let riff_len = 36 + data_len;

        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_len.to_le_bytes());
        wav.extend_from_slice(b"WAVE");

        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 16]); // fmt payload contents are irrelevant here.

        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend_from_slice(sample_bytes);

        wav
    }

    #[test]
    fn strips_a_standard_wav_header_before_decoding() {
        // i16 little-endian: 0, 32767, -32768
        let sample_bytes = [0x00, 0x00, 0xff, 0x7f, 0x00, 0x80];
        let wav = wrap_in_wav_header(&sample_bytes);

        let stripped = strip_wav_header(&wav);
        assert_eq!(stripped, sample_bytes);

        let samples = decode_linear16(stripped);
        assert_eq!(samples.len(), 3);
        assert!((samples[0] - 0.0).abs() < 1e-6);
        assert!((samples[1] - 1.0).abs() < 1e-4);
        assert!((samples[2] + 1.0).abs() < 1e-4);
    }

    #[test]
    fn strips_a_wav_header_with_an_extra_subchunk_before_data() {
        let sample_bytes = [0x00, 0x00, 0x01, 0x00];
        let data_len = sample_bytes.len() as u32;

        // An extra "fact" subchunk (as some encoders emit) sits between
        // "fmt " and "data"; the header walker must skip over it using its
        // declared length rather than assuming "data" starts at byte 36.
        let extra_chunk_payload = [0xAA, 0xBB, 0xCC, 0xDD];
        let extra_chunk_len = extra_chunk_payload.len() as u32;

        let riff_len = 4 + (8 + 16) + (8 + extra_chunk_len) + (8 + data_len);

        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_len.to_le_bytes());
        wav.extend_from_slice(b"WAVE");

        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 16]);

        wav.extend_from_slice(b"fact");
        wav.extend_from_slice(&extra_chunk_len.to_le_bytes());
        wav.extend_from_slice(&extra_chunk_payload);

        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend_from_slice(&sample_bytes);

        let stripped = strip_wav_header(&wav);
        assert_eq!(stripped, sample_bytes);
    }

    #[test]
    fn leaves_a_headerless_chunk_unchanged() {
        let sample_bytes = [0x00, 0x00, 0x01, 0x00];
        assert_eq!(strip_wav_header(&sample_bytes), sample_bytes);
    }
}
