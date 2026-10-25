use anyhow::{Context as _, Result, anyhow};
use async_tungstenite::tungstenite::{
    Message,
    client::IntoClientRequest as _,
    http::{HeaderValue, Request, header::AUTHORIZATION},
};
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{App, AppContext as _, Task};
use std::sync::Arc;
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::{ClientEvent, REALTIME_URL, ServerEvent, SessionConfig, decode, encode};

/// One live voice session. Dropping it closes the socket.
///
/// When the session ends for any reason, `events` yields one
/// `ServerEvent::Error("closed: …")` and then ends, so a consumer only has to
/// watch one stream to notice a dead session.
pub struct RealtimeConnection {
    pub sender: mpsc::UnboundedSender<ClientEvent>,
    pub events: mpsc::UnboundedReceiver<ServerEvent>,
    _task: Task<()>,
}

/// The seam between voice mode and a speech-to-speech vendor, so tests can
/// drive voice mode with `FakeRealtime` instead of the network.
pub trait RealtimeVoice: 'static {
    fn connect(&self, config: SessionConfig, cx: &mut App) -> Task<Result<RealtimeConnection>>;
}

pub struct InworldRealtime {
    api_key: String,
}

impl InworldRealtime {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }

    fn request(&self) -> Result<Request<()>> {
        let url = format!(
            "{REALTIME_URL}?key={}&protocol=realtime",
            uuid::Uuid::new_v4()
        );
        let mut request = url.into_client_request()?;
        let mut authorization = HeaderValue::from_str(&format!("Basic {}", self.api_key))
            .context("the Inworld API key is not a valid header value")?;
        authorization.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, authorization);
        Ok(request)
    }
}

impl RealtimeVoice for InworldRealtime {
    fn connect(&self, config: SessionConfig, cx: &mut App) -> Task<Result<RealtimeConnection>> {
        let request = match self.request() {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        let (sender, outgoing) = mpsc::unbounded();
        let (incoming, events) = mpsc::unbounded();
        let (connected_sender, connected) = oneshot::channel();

        // The whole socket lives on Tokio because rustls-over-tokio needs its
        // reactor; dropping the GPUI task aborts it.
        let socket = gpui_tokio::Tokio::spawn(
            cx,
            run_socket(request, config, outgoing, incoming, connected_sender),
        );
        let task = cx.background_spawn(async move {
            if let Err(error) = socket.await {
                log::error!("listen: realtime socket task failed: {error}");
            }
        });

        cx.background_spawn(async move {
            connected
                .await
                .context("the realtime socket ended before connecting")??;
            Ok(RealtimeConnection {
                sender,
                events,
                _task: task,
            })
        })
    }
}

async fn run_socket(
    request: Request<()>,
    config: SessionConfig,
    outgoing: mpsc::UnboundedReceiver<ClientEvent>,
    incoming: mpsc::UnboundedSender<ServerEvent>,
    connected: oneshot::Sender<Result<()>>,
) {
    let connector = Arc::new(http_client_tls::tls_config()).into();
    let socket =
        match async_tungstenite::tokio::connect_async_with_tls_connector(request, Some(connector))
            .await
        {
            Ok((socket, _response)) => socket,
            Err(error) => {
                connected
                    .send(Err(
                        anyhow!(error).context("connecting to Inworld Realtime failed")
                    ))
                    .ok();
                return;
            }
        };
    if connected.send(Ok(())).is_err() {
        return;
    }
    let reason = pump(socket, config, outgoing, &incoming).await;
    incoming
        .unbounded_send(ServerEvent::Error(format!("closed: {reason}")))
        .ok();
}

/// Shuttles frames both ways until the socket closes, returning why.
async fn pump(
    socket: async_tungstenite::WebSocketStream<async_tungstenite::tokio::ConnectStream>,
    config: SessionConfig,
    mut outgoing: mpsc::UnboundedReceiver<ClientEvent>,
    incoming: &mpsc::UnboundedSender<ServerEvent>,
) -> String {
    let (mut writer, mut reader) = socket.split();
    let mut config = Some(config);
    loop {
        let outbound = futures::select! {
            message = reader.next().fuse() => match message {
                None => return "the server ended the stream".into(),
                Some(Err(error)) => return error.to_string(),
                Some(Ok(Message::Close(frame))) => {
                    return match frame {
                        Some(frame) => format!("{} {}", u16::from(frame.code), frame.reason),
                        None => "the server closed the socket".into(),
                    };
                }
                Some(Ok(Message::Text(text))) => {
                    let event = match decode(text.as_str()) {
                        Ok(event) => event,
                        Err(error) => {
                            log::warn!("listen: skipping an undecodable realtime frame: {error:#}");
                            continue;
                        }
                    };
                    // The server ignores a session.update sent before it has
                    // created the session, so the config waits for this.
                    let session_update = if event == ServerEvent::SessionCreated {
                        config.take().map(ClientEvent::SessionUpdate)
                    } else {
                        None
                    };
                    if incoming.unbounded_send(event).is_err() {
                        return "nobody is listening".into();
                    }
                    match session_update {
                        Some(update) => update,
                        None => continue,
                    }
                }
                Some(Ok(_)) => continue,
            },
            event = outgoing.next() => match event {
                Some(event) => event,
                None => {
                    if let Err(error) = writer.send(Message::Close(None)).await {
                        log::warn!("listen: closing the realtime socket failed: {error}");
                    }
                    return "the connection was dropped".into();
                }
            },
        };
        let frame = match encode(&outbound) {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!("listen: skipping an unencodable realtime event: {error:#}");
                continue;
            }
        };
        if let Err(error) = writer.send(Message::text(frame)).await {
            return error.to_string();
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct FakeRealtimeState {
    sent: Vec<ClientEvent>,
    current: Option<mpsc::UnboundedSender<ServerEvent>>,
    connections: usize,
    configs: Vec<SessionConfig>,
    fail_next: Option<String>,
    hang_next: bool,
}

/// Test double. Each `connect` hands back a connection whose other end the
/// test drives through `emit`, `sent`, and `drop_connection`.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Default)]
pub struct FakeRealtime {
    state: Arc<Mutex<FakeRealtimeState>>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeRealtime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything sent on any connection, in order.
    pub fn sent(&self) -> Vec<ClientEvent> {
        lock(&self.state).sent.clone()
    }

    /// Delivers `event` on the most recent connection.
    pub fn emit(&self, event: ServerEvent) {
        match lock(&self.state).current.as_ref() {
            Some(current) => {
                current.unbounded_send(event).ok();
            }
            None => log::warn!("listen: FakeRealtime emitted with no connection open"),
        }
    }

    /// Ends the most recent connection the way a dropped socket would. What
    /// is sent on it afterwards is not recorded, as a dead socket delivers
    /// nothing.
    pub fn drop_connection(&self) {
        if let Some(current) = lock(&self.state).current.take() {
            current
                .unbounded_send(ServerEvent::Error(
                    "closed: FakeRealtime dropped the connection".into(),
                ))
                .ok();
        }
    }

    /// How many connects have succeeded.
    pub fn connections(&self) -> usize {
        lock(&self.state).connections
    }

    /// The config of every connect attempt that succeeded, in order.
    pub fn configs(&self) -> Vec<SessionConfig> {
        lock(&self.state).configs.clone()
    }

    pub fn fail_next_connect(&self, message: &str) {
        lock(&self.state).fail_next = Some(message.to_string());
    }

    /// The next connect never finishes, like a socket stuck opening.
    pub fn hang_next_connect(&self) {
        lock(&self.state).hang_next = true;
    }
}

#[cfg(any(test, feature = "test-support"))]
fn lock(state: &Mutex<FakeRealtimeState>) -> MutexGuard<'_, FakeRealtimeState> {
    // A test that panicked mid-update already failed; keep the state usable.
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(any(test, feature = "test-support"))]
impl RealtimeVoice for FakeRealtime {
    fn connect(&self, config: SessionConfig, cx: &mut App) -> Task<Result<RealtimeConnection>> {
        let mut state = lock(&self.state);
        if let Some(message) = state.fail_next.take() {
            return Task::ready(Err(anyhow!(message)));
        }
        if std::mem::take(&mut state.hang_next) {
            return cx.background_spawn(futures::future::pending());
        }
        state.connections += 1;
        state.configs.push(config);
        let connection_number = state.connections;
        let (sender, mut outgoing) = mpsc::unbounded();
        let (incoming, events) = mpsc::unbounded();
        state.current = Some(incoming);
        drop(state);

        let task = cx.background_spawn({
            let state = self.state.clone();
            async move {
                while let Some(event) = outgoing.next().await {
                    let mut state = lock(&state);
                    let live = state.current.is_some() && state.connections == connection_number;
                    if live {
                        state.sent.push(event);
                    }
                }
            }
        });
        Task::ready(Ok(RealtimeConnection {
            sender,
            events,
            _task: task,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientEvent, ServerEvent, SessionConfig, TurnEagerness};
    use gpui::TestAppContext;

    fn config() -> SessionConfig {
        SessionConfig {
            model: "google/gemini-3.1-flash-lite".into(),
            voice: "Clive".into(),
            instructions: "Be brief.".into(),
            tools: Vec::new(),
            eagerness: TurnEagerness::Medium,
        }
    }

    #[gpui::test]
    async fn fake_connection_delivers_events_and_records_sends(cx: &mut TestAppContext) {
        let fake = FakeRealtime::new();
        let mut connection = cx.update(|cx| fake.connect(config(), cx)).await.unwrap();

        fake.emit(ServerEvent::SessionCreated);
        assert_eq!(
            connection.events.next().await,
            Some(ServerEvent::SessionCreated)
        );

        connection
            .sender
            .unbounded_send(ClientEvent::UserText("hello".into()))
            .unwrap();
        cx.run_until_parked();

        assert_eq!(fake.sent(), vec![ClientEvent::UserText("hello".into())]);
        assert_eq!(fake.connections(), 1);
    }

    #[gpui::test]
    async fn dropping_the_fake_connection_ends_events_with_an_error(cx: &mut TestAppContext) {
        let fake = FakeRealtime::new();
        let mut connection = cx.update(|cx| fake.connect(config(), cx)).await.unwrap();

        fake.drop_connection();

        match connection.events.next().await {
            Some(ServerEvent::Error(message)) => assert!(message.starts_with("closed: ")),
            other => panic!("expected a closing error, got {other:?}"),
        }
        assert_eq!(connection.events.next().await, None);
    }

    #[gpui::test]
    async fn the_fake_stops_recording_sends_to_a_dropped_connection(cx: &mut TestAppContext) {
        let fake = FakeRealtime::new();
        let first = cx.update(|cx| fake.connect(config(), cx)).await.unwrap();
        fake.drop_connection();
        first
            .sender
            .unbounded_send(ClientEvent::UserText("lost".into()))
            .unwrap();
        let second = cx.update(|cx| fake.connect(config(), cx)).await.unwrap();
        second
            .sender
            .unbounded_send(ClientEvent::UserText("heard".into()))
            .unwrap();
        cx.run_until_parked();

        assert_eq!(fake.sent(), vec![ClientEvent::UserText("heard".into())]);
        assert_eq!(fake.configs().len(), 2);
    }

    #[gpui::test]
    async fn the_fake_can_fail_the_next_connect(cx: &mut TestAppContext) {
        let fake = FakeRealtime::new();
        fake.fail_next_connect("no network");

        let error = cx
            .update(|cx| fake.connect(config(), cx))
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "no network");
        assert_eq!(fake.connections(), 0);

        assert!(cx.update(|cx| fake.connect(config(), cx)).await.is_ok());
        assert_eq!(fake.connections(), 1);
    }

    #[gpui::test]
    #[ignore = "needs INWORLD_API_KEY and the network"]
    async fn inworld_realtime_connects_with_a_real_key(cx: &mut TestAppContext) {
        let api_key = std::env::var("INWORLD_API_KEY").expect("INWORLD_API_KEY is not set");
        cx.executor().allow_parking();
        cx.update(gpui_tokio::init);

        let started = std::time::Instant::now();
        let realtime = InworldRealtime::new(api_key);
        let mut connection = cx
            .update(|cx| realtime.connect(config(), cx))
            .await
            .unwrap();
        let first = futures::select! {
            event = connection.events.next() => event,
            _ = futures::FutureExt::fuse(cx.background_executor.timer(std::time::Duration::from_secs(5))) => None,
        };

        assert_eq!(first, Some(ServerEvent::SessionCreated));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        // The session.update sent in reply must be accepted, not answered
        // with an error or a closed socket.
        let reply = connection.events.next().await;
        assert!(
            !matches!(reply, None | Some(ServerEvent::Error(_))),
            "session.update was rejected: {reply:?}"
        );
    }
    /// Barge-in sends `response.cancel` and `conversation.item.truncate`
    /// mid-reply; neither shape is in the recorded fixtures.
    #[gpui::test]
    #[ignore = "needs INWORLD_API_KEY and the network"]
    async fn inworld_accepts_cancel_and_truncate_mid_reply(cx: &mut TestAppContext) {
        let api_key = std::env::var("INWORLD_API_KEY").expect("INWORLD_API_KEY is not set");
        cx.executor().allow_parking();
        cx.update(gpui_tokio::init);
        let realtime = InworldRealtime::new(api_key);
        let mut connection = cx
            .update(|cx| realtime.connect(config(), cx))
            .await
            .unwrap();
        let next = async |connection: &mut RealtimeConnection, seconds: u64| {
            futures::select! {
                event = connection.events.next() => event,
                _ = futures::FutureExt::fuse(cx.background_executor.timer(std::time::Duration::from_secs(seconds))) => None,
            }
        };
        assert_eq!(
            next(&mut connection, 5).await,
            Some(ServerEvent::SessionCreated)
        );
        let send = |event| connection.sender.unbounded_send(event).unwrap();
        send(ClientEvent::UserText(
            "Count slowly from one to twenty.".into(),
        ));
        send(ClientEvent::ResponseCreate);

        let (item_id, content_index, samples) = loop {
            match next(&mut connection, 10).await {
                Some(ServerEvent::AudioDelta {
                    item_id,
                    content_index,
                    samples,
                }) => break (item_id, content_index, samples.len()),
                Some(ServerEvent::Error(message)) => panic!("error before audio: {message}"),
                Some(_) => continue,
                None => panic!("no audio arrived"),
            }
        };
        let audio_end_ms = u32::try_from(samples * 1_000 / 24_000).unwrap();
        connection
            .sender
            .unbounded_send(ClientEvent::ResponseCancel)
            .unwrap();
        connection
            .sender
            .unbounded_send(ClientEvent::Truncate {
                item_id,
                content_index,
                audio_end_ms,
            })
            .unwrap();

        let mut seen = Vec::new();
        while let Some(event) = next(&mut connection, 3).await {
            if let ServerEvent::Error(message) = &event {
                panic!("cancel or truncate was rejected: {message} (after {seen:?})");
            }
            if !matches!(event, ServerEvent::AudioDelta { .. }) {
                seen.push(event);
            }
        }
        eprintln!("events after cancel and truncate: {seen:?}");

        // Voice mode re-sends the whole session config mid-conversation to
        // refresh the state block.
        let mut refreshed = config();
        refreshed.instructions = "Be brief. Open sessions: none.".into();
        connection
            .sender
            .unbounded_send(ClientEvent::SessionUpdate(refreshed))
            .unwrap();
        let mut seen = Vec::new();
        while let Some(event) = next(&mut connection, 3).await {
            if let ServerEvent::Error(message) = &event {
                panic!(
                    "a mid-conversation session.update was rejected: {message} (after {seen:?})"
                );
            }
            seen.push(event);
        }
        eprintln!("events after a mid-conversation session.update: {seen:?}");
    }
}
