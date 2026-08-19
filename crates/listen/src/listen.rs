mod provider;

#[cfg(any(test, feature = "test-support"))]
pub use provider::FakeStt;
pub use provider::{SttProvider, Transcript};
