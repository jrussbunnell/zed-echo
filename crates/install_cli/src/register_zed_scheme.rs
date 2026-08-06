use client::ZED_URL_SCHEME;
use gpui::{AsyncApp, actions};
use release_channel::ReleaseChannel;

actions!(
    cli,
    [
        /// Registers the zed:// URL scheme handler.
        RegisterZedScheme
    ]
);

/// Registers the `zed://` URL scheme with the OS.
///
/// This is a no-op on [`ReleaseChannel::Dev`], which this fork (Zed Echo)
/// reuses: its bundle already declares `zedecho://` in Info.plist, and it
/// must never re-claim `zed://` from the real Zed in LaunchServices. Gating
/// here (rather than at each call site) covers every current and future
/// caller, including the `RegisterZedScheme` palette action.
pub async fn register_zed_scheme(cx: &AsyncApp) -> anyhow::Result<()> {
    if cx.update(|cx| ReleaseChannel::global(cx)) == ReleaseChannel::Dev {
        log::info!("skipping zed:// scheme registration on the Dev channel (Zed Echo)");
        return Ok(());
    }
    cx.update(|cx| cx.register_url_scheme(ZED_URL_SCHEME)).await
}
