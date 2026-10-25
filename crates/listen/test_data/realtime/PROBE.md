# Inworld Realtime probe (2026-10-25)

Real events from `wss://api.inworld.ai/api/v1/realtime/session`. Fixtures are
server events only. `output_audio_delta.json` has its base64 truncated to 176
chars. `error.json` is the server's answer to a retired model id.

## Setup

- Auth: `Authorization: Basic <key>`. The key is in the macOS keychain as an
  internet password with server `https://api.inworld.ai` (scheme included;
  `-s api.inworld.ai` does not match).
- Voice `Clive`, input 24 kHz PCM16, 80 ms appends.
- Speech: `say "what time is it"`, 0.75 s of speech, then silence.
- Models are listed by `GET https://api.inworld.ai/llm/v1alpha/models`
  (Basic auth). Realtime takes the id as `provider/model`, e.g.
  `google/gemini-3.1-flash-lite` (the list shows `gemini-3.1-flash-lite`).

## Latency (audio-only session, semantic_vad eagerness=high)

"End of speech" is when the last speech sample was sent. "Stopped" is the
server's `input_audio_buffer.speech_stopped`.

| Model | TTS | First audio after end of speech | After `speech_stopped` |
|---|---|---|---|
| openai/gpt-4o-mini | tts-2 | 1.19 - 1.21 s | 0.55 - 0.57 s |
| google/gemini-2.5-flash | tts-2 | 1.05 s | 0.42 s |
| google/gemini-2.5-flash-lite | tts-2 | 1.02 s | 0.39 s |
| google/gemini-3.1-flash-lite | tts-2 | 1.00 s | 0.37 s |
| openai/gpt-5.4-nano | tts-2 | 1.31 s | 0.69 s |
| google/gemini-3.1-flash-lite | tts-2-flash | 0.94 - 1.00 s (4 runs) | 0.33 - 0.36 s |
| openai/gpt-4.1-nano | - | error: `retired on 2026-10-23` | - |
| inworld/latency-optimizer-ab-test | - | error: `Router not found: latency-optimizer-ab-test` | - |

About 0.6 s of every figure is the server waiting to decide the turn ended
(`speech_stopped` arrives about 0.6 s after the last speech sample). It barely
moved with `providerData.stt` overrides (`min_end_of_turn_silence` 20-100,
`max_turn_silence` 100-300, `end_of_turn_confidence_threshold` 0.3-0.5).

## Chosen default

`google/gemini-3.1-flash-lite` with `audio.output.model = inworld-tts-2-flash`.
The gate ("first audio within 1 s of end of speech") is met only at the edge:
0.94 - 1.00 s. Real microphone speech and network may land above 1 s.

## Event names vs the spec table

No differences. Observed exactly as specified:
`session.created`, `session.updated`, `input_audio_buffer.speech_started`,
`input_audio_buffer.speech_stopped`, `conversation.item.input_audio_transcription.delta/.completed`,
`response.output_audio.delta/.done`, `response.output_audio_transcript.delta/.done`,
`response.function_call_arguments.delta/.done`, `response.done`, `error`.

Extras seen: `input_audio_buffer.turn_suggestion`, `input_audio_buffer.committed`,
`response.content_part.added/done`, `response.output_text.done`.

Notes for decoding:

- `speech_started` carries `audio_start_ms` and `item_id`.
- `response.output_audio.delta` carries `item_id` and `content_index` (1 for audio).
- `response.output_audio_transcript.delta` may deliver `"\n\n"` as a delta.
- `response.function_call_arguments.done.arguments` is a JSON string.
- Sending a new audio turn while a response is still playing produces
  `response.done` right after `speech_started` (interrupt works).
- `audio.input.transcription` is null in `session.created`; set it
  (`inworld/inworld-stt-1`) to receive input transcripts.
- The tool-result flow was probed with `function_call_output` followed by an
  explicit `response.create`; whether the server also continues by itself was
  not isolated.

## Rate

Not visible without the Inworld dashboard. Model prices from the list API:
gemini-3.1-flash-lite 2.5e-7 per prompt token.
