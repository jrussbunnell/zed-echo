//! Transcription through Inworld, the same vendor and the same API key the
//! fork already provisions for speech.
//!
//! This uses the synchronous `transcribe` endpoint rather than the
//! bidirectional streaming one.
//!
//! ponytail: the utterance is buffered and sent as one request. A spoken
//! command runs a second or two, so what that costs is the tail of the
//! utterance rather than the whole of it — and it avoids a WebSocket client
//! this crate would be the only user of. The streaming endpoint
//! (`wss://api.inworld.ai/stt/v1/transcribe:streamBidirectional`) is the
//! upgrade if the wait becomes noticeable.

use crate::provider::{SttProvider, Transcript};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use futures::channel::mpsc;
use futures::{AsyncReadExt as _, StreamExt as _};
use gpui::{App, AppContext as _};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use std::sync::Arc;

pub const INWORLD_STT_URL: &str = "https://api.inworld.ai/stt/v1/transcribe";

/// What the microphone stream is resampled to before it gets here.
const SAMPLE_RATE: u32 = 16_000;

const DEFAULT_MODEL_ID: &str = "inworld/inworld-stt-1";

/// Beyond this an utterance is not a command — it is a conversation the
/// recognizer failed to end. Bounding it keeps a stuck endpointer from
/// uploading unbounded audio.
const MAX_UTTERANCE_SAMPLES: usize = SAMPLE_RATE as usize * 60;

pub struct InworldStt {
    client: Arc<dyn HttpClient>,
    api_key: String,
    model_id: String,
}

impl InworldStt {
    pub fn new(client: Arc<dyn HttpClient>, api_key: String) -> Self {
        Self {
            client,
            api_key,
            model_id: DEFAULT_MODEL_ID.to_string(),
        }
    }

    pub fn with_model(mut self, model_id: String) -> Self {
        if !model_id.trim().is_empty() {
            self.model_id = model_id;
        }
        self
    }
}

impl SttProvider for InworldStt {
    fn transcribe(
        &self,
        audio: mpsc::UnboundedReceiver<Vec<f32>>,
        cx: &App,
    ) -> mpsc::UnboundedReceiver<Result<Transcript>> {
        let (sender, receiver) = mpsc::unbounded();
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model_id = self.model_id.clone();

        cx.background_spawn(async move {
            let result = transcribe_utterance(client, api_key, model_id, audio).await;
            sender.unbounded_send(result).ok();
        })
        .detach();

        receiver
    }
}

async fn transcribe_utterance(
    client: Arc<dyn HttpClient>,
    api_key: String,
    model_id: String,
    mut audio: mpsc::UnboundedReceiver<Vec<f32>>,
) -> Result<Transcript> {
    let mut samples: Vec<f32> = Vec::new();
    while let Some(frame) = audio.next().await {
        if samples.len() >= MAX_UTTERANCE_SAMPLES {
            log::warn!("listen: utterance exceeded the bound; transcribing what was captured");
            break;
        }
        samples.extend(frame);
    }
    if samples.is_empty() {
        return Err(anyhow!("no audio was captured"));
    }

    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(INWORLD_STT_URL)
        .header("Authorization", format!("Basic {}", api_key.trim()))
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(serde_json::to_string(&request_body(
            &samples, &model_id,
        ))?))
        .context("building the transcription request")?;

    let mut response = client
        .send(request)
        .await
        .context("sending the transcription request")?;
    let status = response.status();
    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .context("reading the transcription response")?;

    if !status.is_success() {
        return Err(anyhow!(
            "the transcription service returned {status}: {}",
            body.trim()
        ));
    }

    parse_response(&body)
}

fn request_body(samples: &[f32], model_id: &str) -> serde_json::Value {
    serde_json::json!({
        "transcribeConfig": {
            "modelId": model_id,
            "audioEncoding": "LINEAR16",
            "language": "en",
            "sampleRateHertz": SAMPLE_RATE,
            "numberOfChannels": 1,
        },
        "audioData": {
            "content": base64::engine::general_purpose::STANDARD.encode(encode_linear16(samples)),
        },
    })
}

/// LINEAR16 is little-endian signed 16-bit PCM, the inverse of what
/// `read_aloud` decodes on the way back.
fn encode_linear16(samples: &[f32]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let scaled = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        encoded.extend_from_slice(&scaled.to_le_bytes());
    }
    encoded
}

/// An empty transcript is an error rather than an empty command: dispatching
/// one would send nothing to the agent and look like the listener working.
fn parse_response(body: &str) -> Result<Transcript> {
    let value: serde_json::Value =
        serde_json::from_str(body).context("parsing the transcription response")?;

    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
    {
        return Err(anyhow!("the transcription service failed: {message}"));
    }

    let transcription = value
        .get("transcription")
        .ok_or_else(|| anyhow!("the transcription response carried no transcription"))?;
    let text = transcription
        .get("transcript")
        .and_then(|transcript| transcript.as_str())
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Err(anyhow!("nothing was transcribed"));
    }

    Ok(Transcript {
        text: text.to_string(),
        // The synchronous endpoint answers a whole utterance, so its result is
        // final whether or not it says so.
        is_final: transcription
            .get("isFinal")
            .and_then(|is_final| is_final.as_bool())
            .unwrap_or(true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_is_parsed_into_its_text() {
        let body = r#"{"transcription":{"transcript":"check the tests","isFinal":true},
                       "usage":{"transcribedAudioMs":1200}}"#;
        let parsed = parse_response(body).unwrap();
        assert_eq!(parsed.text, "check the tests");
        assert!(parsed.is_final);
    }

    /// The synchronous endpoint answers a whole utterance, so a response that
    /// omits the flag is still something to act on.
    #[test]
    fn a_response_without_the_final_flag_is_treated_as_final() {
        let body = r#"{"transcription":{"transcript":"approve"}}"#;
        assert!(parse_response(body).unwrap().is_final);
    }

    /// Dispatching an empty transcript would send nothing to the agent while
    /// looking exactly like the listener working.
    #[test]
    fn an_empty_transcript_is_an_error_rather_than_an_empty_command() {
        assert!(parse_response(r#"{"transcription":{"transcript":"  "}}"#).is_err());
        assert!(parse_response(r#"{"transcription":{}}"#).is_err());
        assert!(parse_response(r#"{"usage":{}}"#).is_err());
    }

    #[test]
    fn an_error_response_is_reported_rather_than_ignored() {
        let body = r#"{"error":{"message":"quota exceeded"}}"#;
        let error = parse_response(body).unwrap_err();
        assert!(format!("{error}").contains("quota exceeded"));
    }

    #[test]
    fn a_body_that_is_not_json_is_an_error() {
        assert!(parse_response("<html>gateway timeout</html>").is_err());
    }

    #[test]
    fn frames_encode_as_little_endian_signed_sixteen_bit() {
        assert_eq!(
            encode_linear16(&[0.0, 1.0, -1.0]),
            vec![0x00, 0x00, 0xFF, 0x7F, 0x01, 0x80]
        );
    }

    #[test]
    fn the_request_names_the_encoding_and_rate_the_audio_actually_is() {
        let body = request_body(&[0.0, 0.5], DEFAULT_MODEL_ID);
        let config = &body["transcribeConfig"];
        assert_eq!(config["audioEncoding"], "LINEAR16");
        assert_eq!(config["sampleRateHertz"], SAMPLE_RATE);
        assert_eq!(config["numberOfChannels"], 1);
        assert_eq!(config["modelId"], DEFAULT_MODEL_ID);
        assert!(
            body["audioData"]["content"]
                .as_str()
                .is_some_and(|content| !content.is_empty())
        );
    }
}
