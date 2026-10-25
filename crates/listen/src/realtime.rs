use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

pub const REALTIME_SAMPLE_RATE: u32 = 24_000;
pub const REALTIME_URL: &str = "wss://api.inworld.ai/api/v1/realtime/session";

const INPUT_TRANSCRIPTION_MODEL: &str = "inworld/inworld-stt-1";
const OUTPUT_MODEL: &str = "inworld-tts-2-flash";

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum TurnEagerness {
    Low,
    Medium,
    #[default]
    High,
}

impl TurnEagerness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfig {
    pub model: String,
    pub voice: String,
    pub instructions: String,
    pub tools: Vec<ToolDefinition>,
    pub eagerness: TurnEagerness,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientEvent {
    SessionUpdate(SessionConfig),
    AppendAudio(Vec<i16>),
    UserText(String),
    FunctionOutput {
        call_id: String,
        output: String,
    },
    ResponseCreate,
    ResponseCancel,
    Truncate {
        item_id: String,
        content_index: u32,
        audio_end_ms: u32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServerEvent {
    SessionCreated,
    SpeechStarted,
    SpeechStopped,
    AudioDelta {
        item_id: String,
        content_index: u32,
        samples: Vec<i16>,
    },
    AssistantTranscriptDelta(String),
    UserTranscriptCompleted(String),
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    ResponseDone,
    Error(String),
    Other,
}

pub fn encode(event: &ClientEvent) -> Result<String> {
    let value = match event {
        // Shapes follow the recorded `session.updated` fixture: the audio
        // format is `{type: "audio/pcm", rate}` and transcription must be set
        // for input transcripts to arrive (PROBE.md).
        ClientEvent::SessionUpdate(config) => json!({
            "type": "session.update",
            "session": {
                "type": "realtime",
                "model": config.model,
                "instructions": config.instructions,
                "audio": {
                    "input": {
                        "format": { "type": "audio/pcm", "rate": REALTIME_SAMPLE_RATE },
                        "transcription": { "model": INPUT_TRANSCRIPTION_MODEL },
                        "turn_detection": {
                            "type": "semantic_vad",
                            "eagerness": config.eagerness.as_str(),
                            "create_response": true,
                            "interrupt_response": true,
                        },
                    },
                    "output": {
                        "format": { "type": "audio/pcm", "rate": REALTIME_SAMPLE_RATE },
                        "voice": config.voice,
                        "model": OUTPUT_MODEL,
                    },
                },
                "tools": config.tools.iter().map(|tool| json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })).collect::<Vec<_>>(),
                // Inworld answers tool outputs on its own by default; voice
                // mode asks for each response itself, and both together
                // would answer twice.
                "providerData": { "auto_tool_response": false },
            },
        }),
        ClientEvent::AppendAudio(samples) => {
            let bytes: Vec<u8> = samples
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect();
            json!({
                "type": "input_audio_buffer.append",
                "audio": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        }
        ClientEvent::UserText(text) => json!({
            "type": "conversation.item.create",
            "item": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": text }],
            },
        }),
        ClientEvent::FunctionOutput { call_id, output } => {
            if call_id.is_empty() {
                bail!("a function output needs the call id of the call it answers");
            }
            json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                },
            })
        }
        ClientEvent::ResponseCreate => json!({ "type": "response.create" }),
        ClientEvent::ResponseCancel => json!({ "type": "response.cancel" }),
        ClientEvent::Truncate {
            item_id,
            content_index,
            audio_end_ms,
        } => json!({
            "type": "conversation.item.truncate",
            "item_id": item_id,
            "content_index": content_index,
            "audio_end_ms": audio_end_ms,
        }),
    };
    Ok(value.to_string())
}

pub fn decode(frame: &str) -> Result<ServerEvent> {
    let value: Value = serde_json::from_str(frame).context("realtime frame is not JSON")?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("realtime frame has no type"))?;
    let text = |field: &str| -> Result<String> {
        value
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("{kind} is missing `{field}`"))
    };
    Ok(match kind {
        "session.created" => ServerEvent::SessionCreated,
        "input_audio_buffer.speech_started" => ServerEvent::SpeechStarted,
        "input_audio_buffer.speech_stopped" => ServerEvent::SpeechStopped,
        "response.output_audio.delta" => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(text("delta")?)
                .context("audio delta is not base64")?;
            if bytes.len() % 2 != 0 {
                bail!("audio delta has an odd number of bytes");
            }
            ServerEvent::AudioDelta {
                item_id: text("item_id")?,
                content_index: value
                    .get("content_index")
                    .and_then(Value::as_u64)
                    .and_then(|index| u32::try_from(index).ok())
                    .ok_or_else(|| anyhow!("audio delta is missing `content_index`"))?,
                samples: bytes
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .collect(),
            }
        }
        "response.output_audio_transcript.delta" => {
            ServerEvent::AssistantTranscriptDelta(text("delta")?)
        }
        "conversation.item.input_audio_transcription.completed" => {
            ServerEvent::UserTranscriptCompleted(text("transcript")?)
        }
        // `arguments` is a JSON string, passed through undecoded.
        "response.function_call_arguments.done" => ServerEvent::FunctionCall {
            call_id: text("call_id")?,
            name: text("name")?,
            arguments: text("arguments")?,
        },
        "response.done" => ServerEvent::ResponseDone,
        "error" => ServerEvent::Error(
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown realtime error")
                .to_owned(),
        ),
        _ => ServerEvent::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_CREATED: &str = include_str!("../test_data/realtime/session_created.json");
    const SPEECH_STARTED: &str = include_str!("../test_data/realtime/speech_started.json");
    const AUDIO_DELTA: &str = include_str!("../test_data/realtime/output_audio_delta.json");
    const TRANSCRIPT_DELTA: &str =
        include_str!("../test_data/realtime/output_audio_transcript_delta.json");
    const INPUT_TRANSCRIPT: &str =
        include_str!("../test_data/realtime/input_transcription_completed.json");
    const FUNCTION_CALL: &str =
        include_str!("../test_data/realtime/function_call_arguments_done.json");
    const RESPONSE_DONE: &str = include_str!("../test_data/realtime/response_done.json");
    const ERROR: &str = include_str!("../test_data/realtime/error.json");
    const SESSION_UPDATED: &str = include_str!("../test_data/realtime/session_updated.json");

    fn config(eagerness: TurnEagerness) -> SessionConfig {
        SessionConfig {
            model: "google/gemini-3.1-flash-lite".into(),
            voice: "Clive".into(),
            instructions: "Be brief.".into(),
            tools: vec![ToolDefinition {
                name: "ping",
                description: "Ping",
                parameters: serde_json::json!({"type": "object"}),
            }],
            eagerness,
        }
    }

    fn encoded(event: &ClientEvent) -> serde_json::Value {
        serde_json::from_str(&encode(event).unwrap()).unwrap()
    }

    #[test]
    fn every_recorded_server_event_decodes() {
        assert!(matches!(
            decode(SESSION_CREATED).unwrap(),
            ServerEvent::SessionCreated
        ));
        assert!(matches!(
            decode(SESSION_UPDATED).unwrap(),
            ServerEvent::Other
        ));
        assert!(matches!(
            decode(SPEECH_STARTED).unwrap(),
            ServerEvent::SpeechStarted
        ));
        assert!(matches!(
            decode(r#"{"type":"input_audio_buffer.speech_stopped"}"#).unwrap(),
            ServerEvent::SpeechStopped
        ));
        let ServerEvent::AudioDelta {
            item_id,
            content_index,
            samples,
        } = decode(AUDIO_DELTA).unwrap()
        else {
            panic!("not an audio delta");
        };
        assert_eq!(item_id, "ccac0fc8-e311-47c8-9ad9-0a283ac5466b");
        assert_eq!(content_index, 1);
        assert_eq!(samples.len(), 66);
        let ServerEvent::AssistantTranscriptDelta(text) = decode(TRANSCRIPT_DELTA).unwrap() else {
            panic!("not a transcript delta");
        };
        assert_eq!(text, "Hi.");
        let ServerEvent::UserTranscriptCompleted(text) = decode(INPUT_TRANSCRIPT).unwrap() else {
            panic!("not a user transcript");
        };
        assert_eq!(text, "What time is it?");
        let ServerEvent::FunctionCall {
            call_id,
            name,
            arguments,
        } = decode(FUNCTION_CALL).unwrap()
        else {
            panic!("not a function call");
        };
        assert!(!call_id.is_empty());
        assert_eq!(name, "ping");
        assert_eq!(arguments, r#"{"message": "hello"}"#);
        assert!(matches!(
            decode(RESPONSE_DONE).unwrap(),
            ServerEvent::ResponseDone
        ));
        let ServerEvent::Error(message) = decode(ERROR).unwrap() else {
            panic!("not an error");
        };
        assert!(message.contains("retired"));
    }

    #[test]
    fn audio_round_trips_through_base64_pcm16() {
        let samples = vec![0i16, 1, -1, i16::MAX, i16::MIN];
        let json = encoded(&ClientEvent::AppendAudio(samples.clone()));
        assert_eq!(json["type"], "input_audio_buffer.append");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(json["audio"].as_str().unwrap())
            .unwrap();
        let decoded: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(decoded, samples);

        let delta = serde_json::json!({
            "type": "response.output_audio.delta",
            "item_id": "item",
            "content_index": 1,
            "delta": json["audio"],
        });
        let ServerEvent::AudioDelta { samples: back, .. } = decode(&delta.to_string()).unwrap()
        else {
            panic!("not an audio delta");
        };
        assert_eq!(back, samples);
    }

    #[test]
    fn an_odd_length_audio_payload_is_an_error() {
        let delta = serde_json::json!({
            "type": "response.output_audio.delta",
            "item_id": "item",
            "content_index": 1,
            "delta": base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]),
        });
        assert!(decode(&delta.to_string()).is_err());
    }

    #[test]
    fn function_output_requires_a_call_id() {
        assert!(
            encode(&ClientEvent::FunctionOutput {
                call_id: String::new(),
                output: "ok".into()
            })
            .is_err()
        );
        let json = encoded(&ClientEvent::FunctionOutput {
            call_id: "call_1".into(),
            output: "ok".into(),
        });
        assert_eq!(json["type"], "conversation.item.create");
        assert_eq!(json["item"]["type"], "function_call_output");
        assert_eq!(json["item"]["call_id"], "call_1");
        assert_eq!(json["item"]["output"], "ok");
    }

    #[test]
    fn session_update_carries_the_pinned_turn_detection() {
        let json = encoded(&ClientEvent::SessionUpdate(config(TurnEagerness::Medium)));
        assert_eq!(json["type"], "session.update");
        let session = &json["session"];
        assert_eq!(session["type"], "realtime");
        assert_eq!(session["model"], "google/gemini-3.1-flash-lite");
        assert_eq!(session["instructions"], "Be brief.");
        let detection = &session["audio"]["input"]["turn_detection"];
        assert_eq!(detection["type"], "semantic_vad");
        assert_eq!(detection["eagerness"], "medium");
        assert_eq!(detection["create_response"], true);
        assert_eq!(detection["interrupt_response"], true);
        assert_eq!(session["audio"]["input"]["format"]["type"], "audio/pcm");
        assert_eq!(session["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(
            session["audio"]["input"]["transcription"]["model"],
            "inworld/inworld-stt-1"
        );
        assert_eq!(session["audio"]["output"]["format"]["rate"], 24000);
        assert_eq!(session["audio"]["output"]["voice"], "Clive");
        assert_eq!(session["audio"]["output"]["model"], "inworld-tts-2-flash");
        assert_eq!(session["tools"][0]["type"], "function");
        assert_eq!(session["tools"][0]["name"], "ping");
        assert_eq!(session["providerData"]["auto_tool_response"], false);
    }

    #[test]
    fn eagerness_encodes_as_configured() {
        for (eagerness, expected) in [
            (TurnEagerness::Low, "low"),
            (TurnEagerness::Medium, "medium"),
            (TurnEagerness::High, "high"),
        ] {
            let json = encoded(&ClientEvent::SessionUpdate(config(eagerness)));
            assert_eq!(
                json["session"]["audio"]["input"]["turn_detection"]["eagerness"],
                expected
            );
        }
        assert_eq!(TurnEagerness::default(), TurnEagerness::High);
    }

    #[test]
    fn simple_client_events_encode() {
        assert_eq!(
            encoded(&ClientEvent::ResponseCreate)["type"],
            "response.create"
        );
        assert_eq!(
            encoded(&ClientEvent::ResponseCancel)["type"],
            "response.cancel"
        );
        let text = encoded(&ClientEvent::UserText("hi".into()));
        assert_eq!(text["type"], "conversation.item.create");
        assert_eq!(text["item"]["role"], "user");
        assert_eq!(text["item"]["content"][0]["type"], "input_text");
        assert_eq!(text["item"]["content"][0]["text"], "hi");
        let truncate = encoded(&ClientEvent::Truncate {
            item_id: "item".into(),
            content_index: 1,
            audio_end_ms: 500,
        });
        assert_eq!(truncate["type"], "conversation.item.truncate");
        assert_eq!(truncate["item_id"], "item");
        assert_eq!(truncate["content_index"], 1);
        assert_eq!(truncate["audio_end_ms"], 500);
    }

    #[test]
    fn an_unknown_event_type_is_other_not_an_error() {
        assert!(matches!(
            decode(r#"{"type":"rate_limits.updated"}"#).unwrap(),
            ServerEvent::Other
        ));
    }
}
