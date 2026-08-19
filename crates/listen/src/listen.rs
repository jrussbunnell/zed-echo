mod intent;
mod provider;

#[cfg(any(test, feature = "test-support"))]
pub use provider::FakeStt;
pub use intent::{VoiceCommand, parse_intent};
pub use provider::{SttProvider, Transcript};
