mod conversation;
mod intent;
mod inworld_streaming;
mod inworld_stt;
mod listener;
mod provider;
mod realtime;
#[cfg(target_os = "macos")]
mod realtime_audio;
mod realtime_session;
mod wake;

pub use conversation::{Activity, DEFAULT_WINDOW, SILENCE_ENDS_UTTERANCE, Window, activity};
pub use intent::{VoiceCommand, parse_intent};
pub use inworld_streaming::{audio_frame, config_frame, parse_frame};
pub use inworld_stt::{INWORLD_STT_URL, InworldStt};
#[cfg(any(test, feature = "test-support"))]
pub use listener::FakeWake;
pub use listener::{COMMAND_TIMEOUT, ListenEvent, Listener, WakeSignal, WakeSource};
#[cfg(any(test, feature = "test-support"))]
pub use provider::FakeStt;
pub use provider::{SttProvider, Transcript};
pub use realtime::*;
#[cfg(target_os = "macos")]
pub use realtime_audio::{
    Microphone, PlayedAudio, VoiceLevels, VoicePlayback, rms, start_microphone, to_realtime_frame,
};
#[cfg(any(test, feature = "test-support"))]
pub use realtime_session::FakeRealtime;
pub use realtime_session::{InworldRealtime, RealtimeConnection, RealtimeVoice};
#[cfg(target_os = "macos")]
pub use wake::SpeechWake;
pub use wake::contains_wake_word;

use settings::{RegisterSetting, Settings};

pub const INWORLD_STT_PROVIDER: &str = "inworld";
pub const SYSTEM_STT_PROVIDER: &str = "system";

/// Every provider this build can actually hear through, for error messages.
pub const SUPPORTED_STT_PROVIDERS: &[&str] = &[
    INWORLD_STT_PROVIDER,
    #[cfg(target_os = "macos")]
    SYSTEM_STT_PROVIDER,
];

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct ListenSettings {
    pub enabled: bool,
    pub provider: String,
    pub wake_word: String,
    pub confirm_approvals: bool,
    pub conversation: bool,
    pub conversation_window: std::time::Duration,
    pub duck_to: f32,
    pub sidecar: bool,
}

impl ListenSettings {
    /// Resolves `listen.provider` to a provider that actually exists.
    ///
    /// Returns `Err` with the offending value when the setting names something
    /// unimplemented, so the caller can tell the user instead of quietly
    /// hearing through Inworld and leaving the schema's promise unkept.
    /// Matching is case-insensitive because the value is hand-written.
    pub fn resolve_provider(&self) -> Result<&'static str, String> {
        let requested = self.provider.trim();
        SUPPORTED_STT_PROVIDERS
            .iter()
            .find(|supported| requested.eq_ignore_ascii_case(supported))
            .copied()
            .ok_or_else(|| self.provider.clone())
    }
}

impl Settings for ListenSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let listen = content.listen.as_ref();
        ListenSettings {
            enabled: listen
                .and_then(|settings| settings.enabled)
                .unwrap_or(false),
            provider: listen
                .and_then(|settings| settings.provider.clone())
                .unwrap_or_else(|| INWORLD_STT_PROVIDER.to_string()),
            wake_word: listen
                .and_then(|settings| settings.wake_word.clone())
                .filter(|word| !word.trim().is_empty())
                .unwrap_or_else(|| "echo".to_string()),
            confirm_approvals: listen
                .and_then(|settings| settings.confirm_approvals)
                .unwrap_or(true),
            conversation: listen
                .and_then(|settings| settings.conversation)
                .unwrap_or(false),
            conversation_window: listen
                .and_then(|settings| settings.conversation_window_seconds)
                .map(std::time::Duration::from_secs)
                .unwrap_or(conversation::DEFAULT_WINDOW),
            // Clamped because it scales playback volume, and a value outside
            // the range arrives unvalidated from a hand-written settings file.
            duck_to: listen
                .and_then(|settings| settings.duck_to)
                .filter(|duck_to| duck_to.is_finite())
                .map(|duck_to| duck_to.clamp(0.0, 1.0))
                .unwrap_or(0.2),
            sidecar: listen.and_then(|settings| settings.sidecar).unwrap_or(true),
        }
    }
}

pub const DEFAULT_REALTIME_MODEL: &str = "google/gemini-3.1-flash-lite";
pub const INWORLD_REALTIME_PROVIDER: &str = "inworld";
const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// How much voice mode says unprompted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Chattiness {
    Quiet,
    #[default]
    Milestones,
    Commentary,
}

impl From<settings::VoiceChattiness> for Chattiness {
    fn from(chattiness: settings::VoiceChattiness) -> Self {
        match chattiness {
            settings::VoiceChattiness::Quiet => Self::Quiet,
            settings::VoiceChattiness::Milestones => Self::Milestones,
            settings::VoiceChattiness::Commentary => Self::Commentary,
        }
    }
}

impl From<settings::VoiceTurnEagerness> for TurnEagerness {
    fn from(eagerness: settings::VoiceTurnEagerness) -> Self {
        match eagerness {
            settings::VoiceTurnEagerness::Low => Self::Low,
            settings::VoiceTurnEagerness::Medium => Self::Medium,
            settings::VoiceTurnEagerness::High => Self::High,
        }
    }
}

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct VoiceModeSettings {
    pub provider: String,
    pub model: String,
    pub voice: Option<String>,
    pub chattiness: Chattiness,
    pub idle_timeout: std::time::Duration,
    pub turn_eagerness: TurnEagerness,
}

impl VoiceModeSettings {
    /// Resolves `voice_mode.provider` to a provider that exists, returning the
    /// offending value otherwise so the caller can tell the user instead of
    /// quietly connecting to Inworld.
    pub fn resolve_provider(&self) -> Result<&'static str, String> {
        let requested = self.provider.trim();
        if requested.eq_ignore_ascii_case(INWORLD_REALTIME_PROVIDER) {
            Ok(INWORLD_REALTIME_PROVIDER)
        } else {
            Err(self.provider.clone())
        }
    }
}

impl Settings for VoiceModeSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let voice_mode = content.voice_mode.as_ref();
        VoiceModeSettings {
            provider: voice_mode
                .and_then(|settings| settings.provider.clone())
                .unwrap_or_else(|| INWORLD_REALTIME_PROVIDER.to_string()),
            model: voice_mode
                .and_then(|settings| settings.model.clone())
                .filter(|model| !model.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_REALTIME_MODEL.to_string()),
            voice: voice_mode
                .and_then(|settings| settings.voice.clone())
                .filter(|voice| !voice.trim().is_empty()),
            chattiness: voice_mode
                .and_then(|settings| settings.chattiness)
                .map(Chattiness::from)
                .unwrap_or_default(),
            idle_timeout: voice_mode
                .and_then(|settings| settings.idle_timeout_seconds)
                .and_then(|seconds| u64::try_from(seconds).ok())
                .filter(|seconds| *seconds > 0)
                .map(std::time::Duration::from_secs)
                .unwrap_or(DEFAULT_IDLE_TIMEOUT),
            turn_eagerness: voice_mode
                .and_then(|settings| settings.turn_eagerness)
                .map(TurnEagerness::from)
                .unwrap_or_default(),
        }
    }
}

pub fn init(cx: &mut gpui::App) {
    ListenSettings::register(cx);
    VoiceModeSettings::register(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_provider(provider: &str) -> ListenSettings {
        ListenSettings {
            enabled: true,
            provider: provider.to_string(),
            wake_word: "echo".to_string(),
            confirm_approvals: true,
            conversation: false,
            conversation_window: conversation::DEFAULT_WINDOW,
            duck_to: 0.2,
            sidecar: true,
        }
    }

    #[test]
    fn a_known_provider_resolves_whatever_its_casing() {
        assert_eq!(
            settings_with_provider("Inworld").resolve_provider(),
            Ok(INWORLD_STT_PROVIDER)
        );
    }

    /// An unknown provider must not quietly become Inworld: the setting would
    /// then promise something the schema does not deliver, and the user would
    /// have no way to tell.
    #[test]
    fn an_unknown_provider_is_an_error_carrying_the_offending_value() {
        assert_eq!(
            settings_with_provider("deepgram").resolve_provider(),
            Err("deepgram".to_string())
        );
    }

    fn voice_mode_from(json: serde_json::Value) -> VoiceModeSettings {
        let voice_mode = serde_json::from_value(json).expect("valid voice_mode block");
        VoiceModeSettings::from_settings(&settings::SettingsContent {
            voice_mode: Some(voice_mode),
            ..Default::default()
        })
    }

    #[test]
    fn voice_mode_defaults() {
        let voice_mode = VoiceModeSettings::from_settings(&settings::SettingsContent::default());
        assert_eq!(voice_mode.provider, "inworld");
        assert_eq!(voice_mode.model, DEFAULT_REALTIME_MODEL);
        assert_eq!(voice_mode.voice, None);
        assert_eq!(voice_mode.chattiness, Chattiness::Milestones);
        assert_eq!(voice_mode.idle_timeout, std::time::Duration::from_secs(300));
        assert_eq!(voice_mode.turn_eagerness, TurnEagerness::High);
    }

    #[test]
    fn a_zero_idle_timeout_falls_back_to_the_default() {
        for seconds in [0, -5] {
            let voice_mode =
                voice_mode_from(serde_json::json!({ "idle_timeout_seconds": seconds }));
            assert_eq!(voice_mode.idle_timeout, std::time::Duration::from_secs(300));
        }
        let voice_mode = voice_mode_from(serde_json::json!({ "idle_timeout_seconds": 60 }));
        assert_eq!(voice_mode.idle_timeout, std::time::Duration::from_secs(60));
    }

    #[test]
    fn an_unknown_voice_mode_provider_is_reported() {
        let voice_mode = voice_mode_from(serde_json::json!({ "provider": "deepgram" }));
        assert_eq!(voice_mode.resolve_provider(), Err("deepgram".to_string()));
        let voice_mode = voice_mode_from(serde_json::json!({ "provider": "Inworld" }));
        assert_eq!(voice_mode.resolve_provider(), Ok(INWORLD_REALTIME_PROVIDER));
    }

    #[test]
    fn turn_eagerness_parses_each_level() {
        for (name, expected) in [
            ("low", TurnEagerness::Low),
            ("medium", TurnEagerness::Medium),
            ("high", TurnEagerness::High),
        ] {
            let voice_mode = voice_mode_from(serde_json::json!({ "turn_eagerness": name }));
            assert_eq!(voice_mode.turn_eagerness, expected);
        }
    }

    #[test]
    fn chattiness_parses_each_level() {
        for (name, expected) in [
            ("quiet", Chattiness::Quiet),
            ("milestones", Chattiness::Milestones),
            ("commentary", Chattiness::Commentary),
        ] {
            let voice_mode = voice_mode_from(serde_json::json!({ "chattiness": name }));
            assert_eq!(voice_mode.chattiness, expected);
        }
    }
}
