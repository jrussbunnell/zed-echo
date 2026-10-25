// Task 11 wires the proactive feed.
#[allow(dead_code)]
mod feed;
// Task 10 wires the tools and the state block.
#[allow(dead_code)]
mod tools;

use crate::AgentPanel;
use anyhow::Result;
use futures::{StreamExt as _, channel::mpsc};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, Global, IntoElement, Render,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use listen::{
    ClientEvent, InworldRealtime, Microphone, RealtimeVoice, ServerEvent, SessionConfig,
    VoiceLevels, VoiceModeSettings, VoicePlayback,
};
use settings::Settings as _;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ui::{Tooltip, VoiceGlow, prelude::*};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Toast, Workspace, notifications::NotificationId};

const PERSONA: &str = "\
You are Echo, pairing with Joshua out loud. Agents in Echo do the work; you talk about it and direct it.
Speak like a colleague: short, plain sentences, one idea at a time. Never read code, commands, paths longer than a filename, SQL, diffs, or tool output aloud. Say what it means instead.
Acknowledge a request immediately, then act through a tool.
When asked about code, or why an agent did something, use ask_sidecar. Don't guess.
Before stopping a session or rejecting an approval, say what you're about to do in one short sentence.
Session events arrive as bracketed notes. Mention one only if it's worth interrupting a quiet room for.";

const CAPTION_LIFETIME: Duration = Duration::from_secs(4);
const CAPTION_FADE: Duration = Duration::from_millis(500);
const GLOW_HEIGHT: Pixels = px(120.);

/// How long queued audio may sit unplayed before the output stream is
/// treated as wedged and reopened, as read-aloud's player does.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);

struct VoiceModeToast;

struct GlobalVoiceMode(Entity<VoiceMode>);

impl Global for GlobalVoiceMode {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceModeState {
    Connecting,
    Live,
    // Task 10's end_voice_mode closes after its goodbye has played.
    #[allow(dead_code)]
    Closing,
}

enum OutputDevice {
    Current,
    IfMoved,
    Reopen,
}

type MicrophoneStart = Task<Result<(Microphone, mpsc::UnboundedReceiver<Vec<i16>>)>>;

/// Everything that touches a sound card, injected so tests never open one.
struct VoiceAudio {
    microphone: Box<dyn Fn(Arc<VoiceLevels>, &mut App) -> MicrophoneStart>,
    playback: Box<dyn Fn(Arc<VoiceLevels>, OutputDevice, &mut App) -> Option<VoicePlayback>>,
    close_sound: Box<dyn Fn(&mut App)>,
}

impl VoiceAudio {
    fn system() -> Self {
        Self {
            microphone: Box::new(|levels, cx| {
                let echo_canceller = audio::Audio::echo_canceller(cx);
                // Opening the device blocks until it is ready.
                cx.background_spawn(async move { listen::start_microphone(echo_canceller, levels) })
            }),
            playback: Box::new(|levels, device, cx| {
                let mixer = match device {
                    OutputDevice::Current => audio::Audio::output_mixer(cx),
                    OutputDevice::IfMoved => audio::Audio::reconnect_output_mixer(cx, false),
                    OutputDevice::Reopen => audio::Audio::reconnect_output_mixer(cx, true),
                }?;
                Some(VoicePlayback::new(mixer, levels))
            }),
            close_sound: Box::new(|cx| audio::Audio::play_sound(audio::Sound::Leave, cx)),
        }
    }
}

struct Caption {
    text: SharedString,
    shown_at: Instant,
}

/// One open voice conversation, bound to one window.
struct Session {
    state: VoiceModeState,
    window: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
    overlay: Entity<VoiceOverlay>,
    levels: Arc<VoiceLevels>,
    sender: Option<mpsc::UnboundedSender<ClientEvent>>,
    playback: Option<VoicePlayback>,
    playback_progress: Option<(u64, Instant)>,
    microphone: Option<Microphone>,
    caption: Option<Caption>,
    /// Whether the caption is Echo's response still being spoken, so
    /// transcript deltas extend it rather than replace it.
    caption_is_echo: bool,
    /// The last error the server reported, so a close that follows it can
    /// say why.
    last_error: Option<String>,
    _connection: Task<()>,
    _microphone_pump: Option<Task<()>>,
    _caption_fade: Option<Task<()>>,
}

impl Session {
    fn send(&self, event: ClientEvent) {
        let Some(sender) = &self.sender else {
            log::warn!("voice mode: dropped an event sent before the connection opened");
            return;
        };
        if sender.unbounded_send(event).is_err() {
            log::warn!("voice mode: dropped an event sent after the connection closed");
        }
    }

    fn status_label(&self) -> &'static str {
        match self.state {
            VoiceModeState::Connecting => "Connecting…",
            VoiceModeState::Closing => "Closing…",
            VoiceModeState::Live => {
                if self
                    .playback
                    .as_ref()
                    .is_some_and(|playback| playback.is_playing())
                {
                    "Echo"
                } else {
                    "Listening"
                }
            }
        }
    }

    fn has_playback_stalled(&mut self, now: Instant) -> bool {
        let Some(playback) = &self.playback else {
            return false;
        };
        if !playback.is_playing() {
            self.playback_progress = None;
            return false;
        }
        let played = playback.played_samples();
        match self.playback_progress {
            Some((last, since)) if last == played => {
                now.saturating_duration_since(since) >= STALL_TIMEOUT
            }
            _ => {
                self.playback_progress = Some((played, now));
                false
            }
        }
    }
}

/// The app-wide real-time voice conversation with Echo. There is one
/// microphone, so there is one of these, held as a global.
pub struct VoiceMode {
    /// `None` connects to Inworld with the key read-aloud uses.
    provider: Option<Rc<dyn RealtimeVoice>>,
    audio: VoiceAudio,
    session: Option<Session>,
    /// Looking up the API key, before anything has opened.
    opening: Option<Task<()>>,
    _window_closed: Subscription,
}

impl VoiceMode {
    fn new(
        provider: Option<Rc<dyn RealtimeVoice>>,
        audio: VoiceAudio,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = cx.weak_entity();
        let window_closed = cx.on_window_closed(move |cx, window_id| {
            this.update(cx, |voice_mode, cx| {
                let bound_here = voice_mode
                    .session
                    .as_ref()
                    .is_some_and(|session| session.window.window_id() == window_id);
                if bound_here {
                    voice_mode.close(None, cx);
                }
            })
            .ok();
        });
        Self {
            provider,
            audio,
            session: None,
            opening: None,
            _window_closed: window_closed,
        }
    }

    fn install(
        provider: Option<Rc<dyn RealtimeVoice>>,
        audio: VoiceAudio,
        cx: &mut App,
    ) -> Entity<Self> {
        let voice_mode = cx.new(|cx| Self::new(provider, audio, cx));
        cx.set_global(GlobalVoiceMode(voice_mode.clone()));
        voice_mode
    }

    #[cfg(test)]
    pub fn new_for_test(provider: Rc<dyn RealtimeVoice>, cx: &mut App) -> Entity<Self> {
        Self::install(Some(provider), test_audio(Rc::default()), cx)
    }

    fn global(cx: &mut App) -> Entity<Self> {
        match cx.try_global::<GlobalVoiceMode>() {
            Some(global) => global.0.clone(),
            None => Self::install(None, VoiceAudio::system(), cx),
        }
    }

    /// Opens voice mode bound to `window`, closes it if it is already bound
    /// there, or moves it there from another window.
    pub fn toggle(window: &mut Window, cx: &mut App) {
        let window = window.window_handle();
        // Deferred so that no window or workspace is mid-update when opening
        // and closing reach into every one of them.
        cx.defer(move |cx| {
            Self::global(cx).update(cx, |voice_mode, cx| voice_mode.toggle_in(window, cx));
        });
    }

    pub fn is_open(cx: &App) -> bool {
        cx.try_global::<GlobalVoiceMode>()
            .is_some_and(|global| global.0.read(cx).session.is_some())
    }

    /// Hands Echo a line to say in the conversation's own voice.
    pub fn note(text: impl Into<SharedString>, cx: &mut App) {
        let Some(voice_mode) = cx
            .try_global::<GlobalVoiceMode>()
            .map(|global| global.0.clone())
        else {
            return;
        };
        let text = text.into();
        voice_mode.update(cx, |voice_mode, _| {
            if let Some(session) = &voice_mode.session {
                session.send(ClientEvent::UserText(text.to_string()));
                session.send(ClientEvent::ResponseCreate);
            }
        });
    }

    fn toggle_in(&mut self, window: AnyWindowHandle, cx: &mut Context<Self>) {
        // A second press while the key is still being looked up cancels.
        if self.opening.take().is_some() {
            return;
        }
        let Some(workspace) = workspace_in(window, cx) else {
            return;
        };
        match &self.session {
            Some(session) if session.window == window => self.close(None, cx),
            Some(_) => self.move_to(window, workspace, cx),
            None => self.begin_opening(window, workspace, cx),
        }
    }

    fn begin_opening(
        &mut self,
        window: AnyWindowHandle,
        workspace: Entity<Workspace>,
        cx: &mut Context<Self>,
    ) {
        let settings = VoiceModeSettings::get_global(cx).clone();
        if let Err(requested) = settings.resolve_provider() {
            show_toast(
                &workspace,
                format!(
                    "Voice mode can't use the provider {requested:?}. Only \"inworld\" is supported."
                ),
                cx,
            );
            return;
        }
        let voice = settings.voice.clone().unwrap_or_else(|| {
            read_aloud::ReadAloudSettings::get_global(cx)
                .voice_id
                .clone()
        });
        let config = SessionConfig {
            model: settings.model.clone(),
            voice,
            instructions: PERSONA.to_string(),
            tools: Vec::new(),
            eagerness: settings.turn_eagerness,
        };
        let api_key = self
            .provider
            .is_none()
            .then(|| read_aloud::resolve_api_key(cx));

        self.opening = Some(cx.spawn(async move |this, cx| {
            let api_key = match api_key {
                Some(api_key) => Some(api_key.await),
                None => None,
            };
            this.update(cx, |this, cx| {
                this.opening = None;
                let provider = match api_key {
                    None => this.provider.clone(),
                    Some(Ok(api_key)) => {
                        Some(Rc::new(InworldRealtime::new(api_key)) as Rc<dyn RealtimeVoice>)
                    }
                    Some(Err(error)) => {
                        show_toast(
                            &workspace,
                            format!("Voice mode needs an Inworld API key. {error}"),
                            cx,
                        );
                        return;
                    }
                };
                if let Some(provider) = provider {
                    this.open(window, workspace, provider, config, cx);
                }
            })
            .log_err();
        }));
    }

    fn open(
        &mut self,
        window: AnyWindowHandle,
        workspace: Entity<Workspace>,
        provider: Rc<dyn RealtimeVoice>,
        config: SessionConfig,
        cx: &mut Context<Self>,
    ) {
        let levels = Arc::new(VoiceLevels::default());
        let overlay = cx.new({
            let voice_mode = cx.weak_entity();
            let levels = levels.clone();
            |cx| VoiceOverlay::new(voice_mode, levels, cx)
        });

        // One voice at a time: narration and the wake listener both yield.
        read_aloud::set_suppressed(true, cx);
        for (_, panel) in panels_by_window(cx) {
            panel.update(cx, |panel, cx| {
                panel.silence_all_narration(cx);
                panel.stop_listening();
            });
        }
        workspace.update(cx, |workspace, cx| {
            workspace.set_voice_overlay(Some(overlay.clone().into()), cx);
        });

        let connect = provider.connect(config, cx);
        let connection = cx.spawn(async move |this, cx| {
            let mut connection = match connect.await {
                Ok(connection) => connection,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.close(
                            Some(format!("Voice mode couldn't connect: {error:#}").into()),
                            cx,
                        );
                    })
                    .log_err();
                    return;
                }
            };
            let sender = connection.sender.clone();
            let stored = this.update(cx, |this, _| {
                if let Some(session) = this.session.as_mut() {
                    session.sender = Some(sender);
                }
            });
            if stored.is_err() {
                return;
            }
            while let Some(event) = connection.events.next().await {
                if this
                    .update(cx, |this, cx| this.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        self.session = Some(Session {
            state: VoiceModeState::Connecting,
            window,
            workspace: workspace.downgrade(),
            overlay,
            levels,
            sender: None,
            playback: None,
            playback_progress: None,
            microphone: None,
            caption: None,
            caption_is_echo: false,
            last_error: None,
            _connection: connection,
            _microphone_pump: None,
            _caption_fade: None,
        });
        cx.notify();
    }

    fn move_to(
        &mut self,
        window: AnyWindowHandle,
        workspace: Entity<Workspace>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if let Some(previous) = session.workspace.upgrade() {
            previous.update(cx, |previous, cx| previous.set_voice_overlay(None, cx));
        }
        let overlay = session.overlay.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.set_voice_overlay(Some(overlay.into()), cx);
        });
        session.window = window;
        session.workspace = workspace.downgrade();
        cx.notify();
    }

    /// Reverses everything opening did. `reason` is shown as a toast when
    /// the mode closed on its own rather than because it was asked to.
    fn close(&mut self, reason: Option<SharedString>, cx: &mut Context<Self>) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        if let Some(playback) = session.playback.as_mut() {
            playback.stop();
        }
        session.levels.set_input(0.);
        session.levels.set_output(0.);
        let reason = reason.map(|reason| match session.last_error.take() {
            Some(error) => SharedString::from(format!("{reason} ({error})")),
            None => reason,
        });
        let workspace = session.workspace.upgrade();
        // Closes the socket and the microphone.
        drop(session);

        match workspace {
            Some(workspace) => workspace.update(cx, |workspace, cx| {
                workspace.set_voice_overlay(None, cx);
                if let Some(reason) = reason {
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<VoiceModeToast>(),
                            reason.to_string(),
                        ),
                        cx,
                    );
                }
            }),
            None => {
                if let Some(reason) = reason {
                    log::error!("voice mode: {reason}");
                }
            }
        }

        read_aloud::set_suppressed(false, cx);
        // Deferred until this entity is no longer mid-update, because starting
        // a listener asks whether voice mode is still open.
        cx.defer(|cx| {
            for (window, panel) in panels_by_window(cx) {
                window
                    .update(cx, |_, window, cx| {
                        panel.update(cx, |panel, cx| panel.start_listening(window, cx));
                    })
                    .log_err();
            }
        });
        (self.audio.close_sound)(cx);
        cx.notify();
    }

    fn handle_event(&mut self, event: ServerEvent, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match event {
            ServerEvent::SessionCreated => {
                session.state = VoiceModeState::Live;
                self.start_microphone(cx);
            }
            ServerEvent::SpeechStarted => {
                // Barge-in: silence Echo at once, and tell the model how much
                // of its reply was actually heard.
                let played = session
                    .playback
                    .as_mut()
                    .filter(|playback| playback.is_playing())
                    .and_then(|playback| playback.stop());
                if let Some(played) = played {
                    session.send(ClientEvent::Truncate {
                        item_id: played.item_id,
                        content_index: played.content_index,
                        audio_end_ms: played.audio_end_ms,
                    });
                }
            }
            ServerEvent::AudioDelta {
                item_id,
                content_index,
                samples,
            } => self.play(&item_id, content_index, samples, cx),
            ServerEvent::AssistantTranscriptDelta(delta) => {
                let text = match (&session.caption, session.caption_is_echo) {
                    (Some(caption), true) => format!("{}{delta}", caption.text),
                    _ => delta,
                };
                session.caption_is_echo = true;
                self.show_caption(text.into(), cx);
            }
            ServerEvent::UserTranscriptCompleted(text) => {
                session.caption_is_echo = false;
                self.show_caption(text.into(), cx);
            }
            ServerEvent::ResponseDone => session.caption_is_echo = false,
            ServerEvent::Error(message) => {
                if let Some(reason) = message.strip_prefix("closed: ") {
                    let reason = format!("Voice mode lost its connection: {reason}");
                    self.close(Some(reason.into()), cx);
                    return;
                }
                log::error!("voice mode: the server reported an error: {message}");
                session.last_error = Some(message);
            }
            ServerEvent::FunctionCall { name, .. } => {
                log::warn!("voice mode: ignoring a call to {name}; no tools are offered yet");
            }
            ServerEvent::SpeechStopped | ServerEvent::Other => {}
        }
        cx.notify();
    }

    fn play(
        &mut self,
        item_id: &str,
        content_index: u32,
        samples: Vec<i16>,
        cx: &mut Context<Self>,
    ) {
        let now = cx.background_executor().now();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let device = if session.playback.is_none() {
            OutputDevice::Current
        } else if session.has_playback_stalled(now) {
            // ponytail: reopens every STALL_TIMEOUT on a device that never
            // plays; cap the retries as read-aloud does if that shows up.
            log::warn!("voice mode: output stopped sounding; reopening it");
            OutputDevice::Reopen
        } else {
            OutputDevice::IfMoved
        };
        // Moving to a new device loses what was queued on the old one; the
        // rest of the reply plays on the new one.
        if let Some(playback) = (self.audio.playback)(session.levels.clone(), device, cx) {
            if let Some(previous) = session.playback.as_mut() {
                previous.stop();
            }
            session.playback = Some(playback);
            session.playback_progress = None;
        }
        match session.playback.as_mut() {
            Some(playback) => playback.append(item_id, content_index, samples),
            None => log::error!("voice mode: no output device to play Echo's voice on"),
        }
    }

    fn start_microphone(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let start = (self.audio.microphone)(session.levels.clone(), cx);
        session._microphone_pump = Some(cx.spawn(async move |this, cx| {
            let (microphone, mut frames) = match start.await {
                Ok(opened) => opened,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        let reason = format!("Voice mode couldn't open the microphone: {error:#}");
                        this.close(Some(reason.into()), cx);
                    })
                    .log_err();
                    return;
                }
            };
            // A session that closed while the device opened drops the
            // microphone here, which stops it.
            let sender = this
                .update(cx, move |this, _| {
                    let session = this.session.as_mut()?;
                    session.microphone = Some(microphone);
                    session.sender.clone()
                })
                .ok()
                .flatten();
            let Some(sender) = sender else {
                return;
            };
            while let Some(frame) = frames.next().await {
                if sender
                    .unbounded_send(ClientEvent::AppendAudio(frame))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn show_caption(&mut self, text: SharedString, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        session.caption = Some(Caption {
            text,
            shown_at: now,
        });
        // The glow redraws every frame anyway, but not under reduced motion.
        session._caption_fade = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CAPTION_LIFETIME).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
    }
}

fn workspace_in(window: AnyWindowHandle, cx: &App) -> Option<Entity<Workspace>> {
    let multi_workspace = window.downcast::<MultiWorkspace>()?;
    multi_workspace
        .read(cx)
        .ok()
        .map(|multi_workspace| multi_workspace.workspace().clone())
}

/// Every agent panel in every window, so the wake listeners of windows other
/// than the bound one also give up the microphone.
fn panels_by_window(cx: &App) -> Vec<(AnyWindowHandle, Entity<AgentPanel>)> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .flat_map(|multi_workspace| {
            let window: AnyWindowHandle = multi_workspace.into();
            multi_workspace
                .read(cx)
                .map(|multi_workspace| {
                    multi_workspace
                        .workspaces()
                        .filter_map(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
                        .map(|panel| (window, panel))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect()
}

fn show_toast(workspace: &Entity<Workspace>, message: String, cx: &mut App) {
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(NotificationId::unique::<VoiceModeToast>(), message),
            cx,
        );
    });
}

/// The glow, caption and status pill along the bottom of the bound window.
pub struct VoiceOverlay {
    voice_mode: WeakEntity<VoiceMode>,
    levels: Arc<VoiceLevels>,
    _observation: Option<Subscription>,
}

impl VoiceOverlay {
    fn new(
        voice_mode: WeakEntity<VoiceMode>,
        levels: Arc<VoiceLevels>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observation = voice_mode
            .upgrade()
            .map(|voice_mode| cx.observe(&voice_mode, |_, _, cx| cx.notify()));
        Self {
            voice_mode,
            levels,
            _observation: observation,
        }
    }
}

impl Render for VoiceOverlay {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = cx.background_executor().now();
        let (label, caption) = self
            .voice_mode
            .upgrade()
            .and_then(|voice_mode| {
                let session = voice_mode.read(cx).session.as_ref()?;
                let caption = session.caption.as_ref().and_then(|caption| {
                    let age = now.saturating_duration_since(caption.shown_at);
                    let remaining = CAPTION_LIFETIME.checked_sub(age)?;
                    let opacity = (remaining.as_secs_f32() / CAPTION_FADE.as_secs_f32()).min(1.);
                    Some((caption.text.clone(), opacity))
                });
                Some((session.status_label(), caption))
            })
            .unwrap_or(("Closing…", None));
        let colors = cx.theme().colors();

        v_flex()
            .w_full()
            .items_center()
            .gap_1()
            .when_some(caption, |this, (text, opacity)| {
                this.child(
                    div()
                        .max_w(px(640.))
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .bg(colors.elevated_surface_background)
                        .opacity(opacity)
                        .child(Label::new(text).size(LabelSize::Small).truncate()),
                )
            })
            .child(
                h_flex()
                    .id("voice-mode-pill")
                    .occlude()
                    .gap_1()
                    .pl_2()
                    .pr_0p5()
                    .py_0p5()
                    .rounded_full()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface_background)
                    .child(Label::new(label).size(LabelSize::Small))
                    .child(
                        IconButton::new("voice-mode-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("End voice mode"))
                            .on_click(|_, window, cx| VoiceMode::toggle(window, cx)),
                    ),
            )
            .child(div().w_full().h(GLOW_HEIGHT).child(VoiceGlow::new(
                "voice-mode-glow",
                self.levels.input(),
                self.levels.output(),
            )))
    }
}

#[cfg(test)]
fn test_audio(microphones_opened: Rc<std::cell::Cell<usize>>) -> VoiceAudio {
    VoiceAudio {
        microphone: Box::new(move |_, _| {
            microphones_opened.set(microphones_opened.get() + 1);
            let (_frames, receiver) = mpsc::unbounded();
            Task::ready(Ok((Microphone::for_test(), receiver)))
        }),
        playback: Box::new(|levels, device, _| match device {
            OutputDevice::Current => Some(VoicePlayback::for_test(levels)),
            OutputDevice::IfMoved | OutputDevice::Reopen => None,
        }),
        close_sound: Box::new(|_| {}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_view::tests::{StubAgentServer, init_test};
    use crate::voice_dispatch::TestListening;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use listen::FakeRealtime;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use std::cell::Cell;
    use std::path::Path;

    struct Setup {
        cx: VisualTestContext,
        workspace: Entity<Workspace>,
        panel: Entity<AgentPanel>,
        realtime: FakeRealtime,
        voice_mode: Entity<VoiceMode>,
        microphones_opened: Rc<Cell<usize>>,
    }

    async fn setup(provider: Option<FakeRealtime>, cx: &mut TestAppContext) -> Setup {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            cx.set_global(TestListening {
                wake: Arc::new(listen::FakeWake::new()),
                stt: Arc::new(listen::FakeStt::new()),
            });
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.listen.get_or_insert_default().enabled = Some(true);
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();
        assert!(panel.read_with(&cx, |panel, _| panel.is_listening()));

        let realtime = FakeRealtime::new();
        let microphones_opened = Rc::new(Cell::new(0));
        let voice_mode = cx.update(|_, cx| {
            let voice_mode = match provider {
                Some(provider) => VoiceMode::new_for_test(Rc::new(provider), cx),
                None => VoiceMode::install(None, test_audio(Rc::default()), cx),
            };
            voice_mode.update(cx, |voice_mode, _| {
                voice_mode.audio = test_audio(microphones_opened.clone());
            });
            voice_mode
        });
        Setup {
            cx,
            workspace,
            panel,
            realtime,
            voice_mode,
            microphones_opened,
        }
    }

    async fn setup_with_fake(cx: &mut TestAppContext) -> Setup {
        let realtime = FakeRealtime::new();
        let mut setup = setup(Some(realtime.clone()), cx).await;
        setup.realtime = realtime;
        setup
    }

    fn toggle(cx: &mut VisualTestContext) {
        cx.update(|window, cx| VoiceMode::toggle(window, cx));
        cx.run_until_parked();
    }

    fn state(setup: &Setup) -> Option<VoiceModeState> {
        setup.voice_mode.read_with(&setup.cx, |voice_mode, _| {
            voice_mode.session.as_ref().map(|session| session.state)
        })
    }

    fn open_live(setup: &mut Setup) {
        toggle(&mut setup.cx);
        setup.realtime.emit(ServerEvent::SessionCreated);
        setup.cx.run_until_parked();
        assert_eq!(state(setup), Some(VoiceModeState::Live));
    }

    #[gpui::test]
    async fn opening_suppresses_narration_and_suspends_listening(cx: &mut TestAppContext) {
        let mut setup = setup_with_fake(cx).await;

        toggle(&mut setup.cx);
        assert!(setup.cx.update(|_, cx| read_aloud::is_suppressed(cx)));
        assert!(
            !setup
                .panel
                .read_with(&setup.cx, |panel, _| panel.is_listening())
        );
        assert_eq!(state(&setup), Some(VoiceModeState::Connecting));
        assert_eq!(setup.microphones_opened.get(), 0);

        setup.realtime.emit(ServerEvent::SessionCreated);
        setup.cx.run_until_parked();
        assert_eq!(state(&setup), Some(VoiceModeState::Live));
        assert_eq!(setup.microphones_opened.get(), 1);
    }

    #[gpui::test]
    async fn closing_restores_narration_and_listening(cx: &mut TestAppContext) {
        let mut setup = setup_with_fake(cx).await;
        open_live(&mut setup);

        toggle(&mut setup.cx);
        assert_eq!(state(&setup), None);
        assert!(!setup.cx.update(|_, cx| read_aloud::is_suppressed(cx)));
        assert!(
            setup
                .panel
                .read_with(&setup.cx, |panel, _| panel.is_listening())
        );
        assert!(!setup.cx.update(|_, cx| VoiceMode::is_open(cx)));
    }

    #[gpui::test]
    async fn toggling_while_connecting_closes_without_opening_the_mic(cx: &mut TestAppContext) {
        let mut setup = setup_with_fake(cx).await;

        toggle(&mut setup.cx);
        assert_eq!(state(&setup), Some(VoiceModeState::Connecting));
        toggle(&mut setup.cx);
        setup.realtime.emit(ServerEvent::SessionCreated);
        setup.cx.run_until_parked();

        assert_eq!(state(&setup), None);
        assert_eq!(setup.microphones_opened.get(), 0);
        assert!(
            setup
                .panel
                .read_with(&setup.cx, |panel, _| panel.is_listening())
        );
    }

    #[gpui::test]
    async fn closing_the_bound_window_closes_voice_mode(cx: &mut TestAppContext) {
        let mut setup = setup_with_fake(cx).await;
        open_live(&mut setup);

        setup.cx.update(|window, _| window.remove_window());
        cx.run_until_parked();

        assert!(!cx.update(|cx| VoiceMode::is_open(cx)));
        assert!(!cx.update(|cx| read_aloud::is_suppressed(cx)));
    }

    #[gpui::test]
    async fn assistant_audio_is_played_and_speech_interrupts_it(cx: &mut TestAppContext) {
        let mut setup = setup_with_fake(cx).await;
        open_live(&mut setup);

        setup.realtime.emit(ServerEvent::AudioDelta {
            item_id: "item_1".into(),
            content_index: 0,
            samples: vec![1_000; 24_000],
        });
        setup.cx.run_until_parked();
        setup.voice_mode.update(&mut setup.cx, |voice_mode, _| {
            let playback = voice_mode
                .session
                .as_ref()
                .and_then(|session| session.playback.as_ref())
                .expect("Echo's audio is playing");
            assert!(playback.is_playing());
            // 300 ms at 24 kHz.
            playback.set_played_samples_for_test(7_200);
        });

        setup.realtime.emit(ServerEvent::SpeechStarted);
        setup.cx.run_until_parked();

        assert!(setup.realtime.sent().contains(&ClientEvent::Truncate {
            item_id: "item_1".into(),
            content_index: 0,
            audio_end_ms: 300,
        }));
        setup.voice_mode.read_with(&setup.cx, |voice_mode, _| {
            let playback = voice_mode
                .session
                .as_ref()
                .and_then(|session| session.playback.as_ref())
                .expect("playback is kept for the next reply");
            assert!(!playback.is_playing());
        });
    }

    #[gpui::test]
    async fn say_to_the_user_becomes_a_note_while_open(cx: &mut TestAppContext) {
        let mut setup = setup_with_fake(cx).await;
        setup.panel.update_in(&mut setup.cx, |panel, window, cx| {
            panel.open_external_thread_with_server(
                Rc::new(StubAgentServer::default_response()),
                window,
                cx,
            );
        });
        setup.cx.run_until_parked();
        let thread_view = setup
            .panel
            .read_with(&setup.cx, |panel, cx| panel.active_thread_view(cx))
            .expect("a thread is open");
        let tts = read_aloud::FakeTts::new();
        let reader = setup.cx.new({
            let tts = tts.clone();
            |cx| {
                read_aloud::ReadAloud::for_test(
                    Arc::new(tts),
                    Box::new(read_aloud::FakeSink::new()),
                    cx,
                )
            }
        });
        thread_view.update(&mut setup.cx, |view, _| {
            view.set_read_aloud_for_test(reader)
        });
        open_live(&mut setup);

        setup.panel.update(&mut setup.cx, |panel, cx| {
            panel.say_to_the_user("Approved.", cx)
        });
        setup.cx.run_until_parked();

        let sent = setup.realtime.sent();
        let note = sent
            .iter()
            .position(|event| *event == ClientEvent::UserText("Approved.".into()))
            .expect("the line was sent as a note");
        assert_eq!(sent.get(note + 1), Some(&ClientEvent::ResponseCreate));
        assert!(tts.spoken().is_empty());
    }

    #[gpui::test]
    async fn no_api_key_shows_a_toast_and_stays_closed(cx: &mut TestAppContext) {
        if std::env::var("INWORLD_API_KEY").is_ok() {
            eprintln!("skipped: INWORLD_API_KEY is set, so a real connection would be made");
            return;
        }
        let mut setup = setup(None, cx).await;

        toggle(&mut setup.cx);

        assert!(!setup.cx.update(|_, cx| VoiceMode::is_open(cx)));
        assert!(!setup.cx.update(|_, cx| read_aloud::is_suppressed(cx)));
        assert!(
            setup
                .panel
                .read_with(&setup.cx, |panel, _| panel.is_listening())
        );
        assert!(setup.workspace.read_with(&setup.cx, |workspace, _| {
            workspace.has_notification(&NotificationId::unique::<VoiceModeToast>())
        }));
    }
}
