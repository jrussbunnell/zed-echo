mod intent;
mod inworld_stt;
mod listener;
mod provider;
mod wake;

pub use intent::{VoiceCommand, parse_intent};
pub use inworld_stt::{INWORLD_STT_URL, InworldStt};
#[cfg(any(test, feature = "test-support"))]
pub use listener::FakeWake;
pub use listener::{COMMAND_TIMEOUT, ListenEvent, Listener, ListenerState, WakeSignal, WakeSource};
#[cfg(any(test, feature = "test-support"))]
pub use provider::FakeStt;
pub use provider::{SttProvider, Transcript};
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
        }
    }
}

pub fn init(cx: &mut gpui::App) {
    ListenSettings::register(cx);
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
}
