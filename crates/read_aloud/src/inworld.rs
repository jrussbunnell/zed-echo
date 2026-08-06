use crate::provider::{Pcm, TtsProvider};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Task};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use std::sync::Arc;

pub const INWORLD_API_URL: &str = "https://api.inworld.ai/tts/v1/voice:stream";
pub const INWORLD_CREDENTIALS_URL: &str = "https://api.inworld.ai";
const INWORLD_API_KEY_VAR: &str = "INWORLD_API_KEY";
const SAMPLE_RATE: u32 = 22050;
const RATE_LIMIT_MAX_RETRIES: u32 = 2;
const RATE_LIMIT_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

pub struct InworldTts {
    client: Arc<dyn HttpClient>,
    api_key: String,
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
            voice_id,
            model_id,
        }
    }
}

impl TtsProvider for InworldTts {
    fn synthesize(&self, text: String, cx: &App) -> Task<Result<Pcm>> {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let voice_id = self.voice_id.clone();
        let model_id = self.model_id.clone();

        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let body = serde_json::json!({
                "text": text,
                "voiceId": voice_id,
                "modelId": model_id,
                "audioConfig": {
                    "audioEncoding": "LINEAR16",
                    "sampleRateHertz": SAMPLE_RATE,
                },
                "deliveryMode": "BALANCED",
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
                let mut text_body = String::new();
                response.body_mut().read_to_string(&mut text_body).await?;

                if status.is_success() {
                    return Ok(Pcm {
                        samples: collect_audio_content(&text_body)?,
                        sample_rate: SAMPLE_RATE,
                        channels: 1,
                    });
                }

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
                    text_body.trim()
                ));
            }

            Err(anyhow!("Inworld TTS exhausted rate-limit retries"))
        })
    }
}

/// Walks the streamed JSON-lines body, concatenating every `result.audioContent`
/// chunk. Malformed lines are skipped rather than failing the whole utterance.
fn collect_audio_content(body: &str) -> Result<Vec<f32>> {
    let mut samples = Vec::new();
    let mut found_any = false;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(encoded) = value
            .get("result")
            .and_then(|result| result.get("audioContent"))
            .and_then(|content| content.as_str())
        else {
            continue;
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("Inworld returned audioContent that is not valid base64")?;
        samples.extend(decode_linear16(&decoded));
        found_any = true;
    }

    if !found_any {
        return Err(anyhow!("Inworld response contained no audio content"));
    }
    Ok(samples)
}

/// LINEAR16 is little-endian signed 16-bit PCM. A trailing odd byte is not a
/// sample and is discarded.
fn decode_linear16(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
        .collect()
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
        let (_username, secret) = credentials
            .await?
            .context("No Inworld API key found. Set INWORLD_API_KEY or store one in the keychain.")?;
        Ok(String::from_utf8(secret)?.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let samples = collect_audio_content(body).unwrap();
        assert_eq!(samples.len(), 2);
    }

    #[test]
    fn tolerates_blank_and_malformed_lines() {
        let body = "\n{\"result\":{\"audioContent\":\"AAA=\"}}\nnot json\n{}\n";
        let samples = collect_audio_content(body).unwrap();
        assert_eq!(samples.len(), 1, "one good line still yields its audio");
    }

    #[test]
    fn errors_when_the_response_contains_no_audio() {
        assert!(collect_audio_content("{}\n").is_err());
    }
}
