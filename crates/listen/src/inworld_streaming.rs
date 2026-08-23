//! The wire format of Inworld's bidirectional transcription stream.
//!
//! # Status
//!
//! The encoders and the parser below are complete and tested. The socket that
//! would carry them is **not wired**, and that is a decision rather than an
//! omission.
//!
//! Three things stack up against it. There is no API key on this machine, so
//! nothing about the exchange can be checked against the real endpoint.
//! `async_tungstenite`'s connector in this workspace runs on tokio, which this
//! crate does not have and which every other user of it reaches through
//! `gpui_tokio` — a real integration, not a call. And what it buys is the tail
//! of one utterance, perhaps half a second, over the buffered provider that
//! already works and is tested.
//!
//! Unverifiable protocol code, behind a new runtime dependency, for half a
//! second, is a bad trade to make unattended. The half that *can* be pinned
//! down is pinned down here, so wiring the socket later is an afternoon
//! against a known-good format rather than a reverse-engineering exercise.
//!
//! Endpoint: `wss://api.inworld.ai/stt/v1/transcribe:streamBidirectional`.
//! The first frame configures the stream; audio frames follow; transcripts
//! come back as they refine.

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;

/// What the microphone chain delivers, and what the config frame must declare.
pub const SAMPLE_RATE: u32 = 16_000;

const DEFAULT_MODEL_ID: &str = "inworld/inworld-stt-1";

/// The opening frame, which the endpoint expects before any audio.
pub fn config_frame(model_id: &str) -> String {
    serde_json::json!({
        "transcribeConfig": {
            "modelId": if model_id.trim().is_empty() { DEFAULT_MODEL_ID } else { model_id },
            "audioEncoding": "LINEAR16",
            "language": "en",
            "sampleRateHertz": SAMPLE_RATE,
            "numberOfChannels": 1,
        }
    })
    .to_string()
}

/// One frame of captured audio.
pub fn audio_frame(samples: &[f32]) -> String {
    let mut encoded = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let scaled = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        encoded.extend_from_slice(&scaled.to_le_bytes());
    }
    serde_json::json!({
        "audioData": {
            "content": base64::engine::general_purpose::STANDARD.encode(encoded),
        }
    })
    .to_string()
}

/// One transcript from the stream.
///
/// `Ok(None)` for a frame carrying no transcript — a keepalive or metadata.
/// An empty transcript would otherwise dispatch an empty command.
pub fn parse_frame(frame: &str) -> Result<Option<crate::Transcript>> {
    let frame = frame.trim();
    if frame.is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_str(frame).context("parsing a transcription frame")?;

    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
    {
        return Err(anyhow!("the transcription service failed: {message}"));
    }

    let Some(transcription) = value.get("transcription") else {
        return Ok(None);
    };
    let Some(text) = transcription
        .get("transcript")
        .and_then(|transcript| transcript.as_str())
    else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(crate::Transcript {
        text: text.to_string(),
        is_final: transcription
            .get("isFinal")
            .and_then(|is_final| is_final.as_bool())
            .unwrap_or(false),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_frame_declares_the_format_the_chain_actually_produces() {
        let frame: serde_json::Value = serde_json::from_str(&config_frame("")).unwrap();
        let config = &frame["transcribeConfig"];
        assert_eq!(config["audioEncoding"], "LINEAR16");
        assert_eq!(config["sampleRateHertz"], SAMPLE_RATE);
        assert_eq!(config["numberOfChannels"], 1);
        assert_eq!(config["modelId"], DEFAULT_MODEL_ID);
    }

    #[test]
    fn a_named_model_overrides_the_default() {
        let frame: serde_json::Value =
            serde_json::from_str(&config_frame("groq/whisper-large-v3")).unwrap();
        assert_eq!(
            frame["transcribeConfig"]["modelId"],
            "groq/whisper-large-v3"
        );
    }

    #[test]
    fn audio_frames_encode_as_little_endian_signed_sixteen_bit() {
        let frame: serde_json::Value =
            serde_json::from_str(&audio_frame(&[0.0, 1.0, -1.0])).unwrap();
        let encoded = frame["audioData"]["content"].as_str().unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(decoded, vec![0x00, 0x00, 0xFF, 0x7F, 0x01, 0x80]);
    }

    #[test]
    fn a_partial_and_a_final_are_told_apart() {
        let partial =
            parse_frame(r#"{"transcription":{"transcript":"check the","isFinal":false}}"#)
                .unwrap()
                .unwrap();
        assert_eq!(partial.text, "check the");
        assert!(!partial.is_final);

        let final_frame =
            parse_frame(r#"{"transcription":{"transcript":"check the tests","isFinal":true}}"#)
                .unwrap()
                .unwrap();
        assert!(final_frame.is_final);
    }

    /// A stream carries keepalives beside transcripts. Treating one as an
    /// empty transcript would dispatch an empty command.
    #[test]
    fn a_frame_with_no_transcript_yields_nothing() {
        assert!(parse_frame(r#"{"transcription":{}}"#).unwrap().is_none());
        assert!(parse_frame(r#"{"keepalive":true}"#).unwrap().is_none());
        assert!(parse_frame("").unwrap().is_none());
        assert!(
            parse_frame(r#"{"transcription":{"transcript":"   "}}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_error_frame_is_reported_rather_than_ignored() {
        let error = parse_frame(r#"{"error":{"message":"quota exceeded"}}"#).unwrap_err();
        assert!(format!("{error}").contains("quota exceeded"));
    }

    #[test]
    fn a_frame_that_is_not_json_is_an_error() {
        assert!(parse_frame("<html>bad gateway</html>").is_err());
    }
}
