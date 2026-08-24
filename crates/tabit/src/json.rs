//! JSON mode: the stdio edge of the session protocol. LF-JSONL in both
//! directions — the client's lines are [`ClientFrame`]s (an `initialize`
//! handshake, then commands), the server's lines are stamped
//! [`EventFrame`]s plus handshake/transport-error control frames. Only
//! protocol bytes reach stdout; human banners stay on stderr.
//!
//! The whole bridge is generic over `BufRead`/`Write` so tests drive it
//! with in-memory buffers instead of process pipes.

use std::io::{BufRead, Write};
use tabit_protocol::EventFrame;
use tabit_protocol::{ClientFrame, PROTOCOL_VERSION, ServerControlFrame, ServerFrame};
use tabit_session::{SessionCommandLink, SessionHost, SessionInfo};
use tokio::sync::mpsc;

/// Serve the backend over `reader`/`writer` until the client closes its
/// input. Returns the process exit code: 0 normally, 1 on a handshake
/// version mismatch (the connection is rejected and closed).
pub async fn serve<R, W>(mut host: SessionHost, reader: R, writer: W) -> i32
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let (writer_tx, writer_rx) = mpsc::unbounded_channel::<ServerFrame>();
    let link = host.command_link();
    let info = host.info().clone();

    // The ack gate: the handshake ack is the transport's first
    // obligation, and events can exist from spawn (startup degradation
    // frames), so the forwarder waits until the reader has sent the
    // ack. A rejected handshake drops the gate's sender — the forwarder
    // ends without forwarding anything (the rejection is the only
    // frame).
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);

    // The reader blocks on lines; the writer is the single owner of
    // stdout, draining one ordered channel so the handshake ack can
    // never land behind an event.
    let reader_tx = writer_tx.clone();
    let reader_task =
        tokio::task::spawn_blocking(move || read_loop(reader, link, reader_tx, &info, gate_tx));
    let writer_task = tokio::spawn(write_loop(writer_rx, writer));

    // The live forwarder: host events reach stdout as they happen, for
    // the whole connection — not only at wind-down. (v1 bug: events
    // accumulated unread in the actor's channel until the client
    // closed stdin; the GUI sat at "queued" forever while the run was
    // already streaming.)
    let forwarder_task = tokio::spawn(forward_events(
        host.take_events(),
        writer_tx.clone(),
        gate_rx,
    ));

    // A panicked reader thread is a broken edge: exit nonzero.
    let exit = reader_task.await.unwrap_or(1);
    // The client is gone (EOF, broken pipe, or a dead reader thread):
    // at a stdio edge that IS frontend death — abort every in-flight
    // run, discard every queue, and wind down (ruled 2026-08: the core
    // dies with the frontend, regardless of state). The door is
    // dropping the host, NOT the polite close: `close_commands` is
    // not a barrier and would route anything still queued on the
    // command channel — starting fresh, unattended runs for a client
    // that is gone (the review round's finding). Dropping the host
    // closes the command channel: the host task sees it empty, and
    // the unrouted commands die unrouted. Interrupted results
    // synthesize on the next open, exactly like a crash.
    host.abort_all();
    drop(host);
    // The forwarder ends when the worker drops the event sender at
    // wind-down; the writer ends when every writer_tx clone is gone.
    drop(writer_tx);
    let _ = forwarder_task.await;
    let _ = writer_task.await;
    exit
}

/// Pump the actor's event stream into the writer channel until the
/// stream ends — gated on the handshake: nothing forwards before the
/// ack has been sent.
async fn forward_events(
    stream: Option<mpsc::UnboundedReceiver<EventFrame>>,
    out: mpsc::UnboundedSender<ServerFrame>,
    mut gate: tokio::sync::watch::Receiver<bool>,
) {
    let Some(mut stream) = stream else {
        return;
    };
    loop {
        if *gate.borrow() {
            break;
        }
        // A dropped gate means the handshake was rejected: the
        // rejection is the only frame the client ever sees.
        if gate.changed().await.is_err() {
            return;
        }
    }
    while let Some(frame) = stream.recv().await {
        let _ = out.send(ServerFrame::Event(frame));
    }
}

/// Parse client lines and act on them until EOF (or a rejected
/// handshake). Blocking, runs on its own thread. Opens `gate` once the
/// ack has been sent, releasing the event forwarder.
fn read_loop<R: BufRead>(
    mut reader: R,
    link: SessionCommandLink,
    out: mpsc::UnboundedSender<ServerFrame>,
    info: &SessionInfo,
    gate: tokio::sync::watch::Sender<bool>,
) -> i32 {
    fn control(out: &mpsc::UnboundedSender<ServerFrame>, frame: ServerControlFrame) {
        let _ = out.send(ServerFrame::Control(frame));
    }

    let mut initialized = false;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return 0, // EOF: the client is done
            Ok(_) => {}
            Err(_) => return 0, // a broken pipe reads as EOF
        }
        let frame = line.trim_end_matches(['\r', '\n']);
        if frame.is_empty() {
            continue;
        }
        match serde_json::from_str::<ClientFrame>(frame) {
            Ok(ClientFrame::Initialize {
                protocol_version,
                replay,
            }) if !initialized => {
                if protocol_version == PROTOCOL_VERSION {
                    control(
                        &out,
                        ServerControlFrame::InitializeAck {
                            protocol_version: PROTOCOL_VERSION,
                            session_id: info.session_id.clone(),
                            session_path: info.session_path.clone(),
                            model: info.model.clone(),
                            resumed: info.resumed,
                        },
                    );
                    // The ack is queued ahead of anything the forwarder
                    // will send: events may flow from here on.
                    let _ = gate.send(true);
                    initialized = true;
                    // The pass streams onto the host's event channel,
                    // so it lands after the ack (the gate just opened)
                    // and after the startup frames already queued on
                    // the same sender.
                    if replay {
                        link.replay(&info.session_id);
                    }
                } else {
                    control(
                        &out,
                        ServerControlFrame::InitializeRejected {
                            reason: format!(
                                "protocol version {PROTOCOL_VERSION} required, \
                                 client sent {protocol_version}"
                            ),
                        },
                    );
                    return 1;
                }
            }
            Ok(ClientFrame::Initialize { .. }) => {
                control(
                    &out,
                    ServerControlFrame::ProtocolError {
                        message: "already initialized".to_string(),
                    },
                );
            }
            Ok(ClientFrame::Command(command)) if initialized => link.send(command),
            Ok(ClientFrame::Command(_)) => {
                control(
                    &out,
                    ServerControlFrame::ProtocolError {
                        message: "command before initialize".to_string(),
                    },
                );
            }
            Err(error) => {
                control(
                    &out,
                    ServerControlFrame::ProtocolError {
                        message: format!("unparseable line: {error}"),
                    },
                );
            }
        }
    }
}

/// Serialize frames one per line. A failing writer means the client is
/// gone: stop writing, the shutdown path ends everything else.
async fn write_loop<W: Write>(mut rx: mpsc::UnboundedReceiver<ServerFrame>, mut writer: W) {
    while let Some(frame) = rx.recv().await {
        let line = tabit_protocol::to_wire_line(&frame);
        if writeln!(writer, "{line}").is_err() {
            break;
        }
    }
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_agent::agent::ModelHandle;
    use rig_agent::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig_core::completion::Usage;
    use std::io::{Cursor, Read};
    use std::path::Path;
    use std::sync::Arc;
    use tabit_config::{AuthConfig, TabitConfig};
    use tabit_session::{
        EventFrame, ModelSelection, Session, SessionBuilder, SessionHost, SessionHostWiring,
        SessionSource, SessionStore,
    };

    fn script(text: &str) -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::text(text),
            MockStreamEvent::final_response(Usage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                ..Usage::default()
            }),
        ]
    }

    /// The test store directory for `tag` (one per test, cleaned at
    /// build).
    fn test_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join("tabit-json-tests")
            .join(format!("{tag}-{}", std::process::id()))
    }

    fn build_session(dir: &Path, turns: Vec<Vec<MockStreamEvent>>) -> Session {
        let config = Arc::new(
            TabitConfig::from_toml_str(
                r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"

[[providers.p.models]]
id = "m"
"#,
                Path::new("providers.toml"),
            )
            .expect("config"),
        );
        let auth = Arc::new(AuthConfig::default());
        SessionBuilder::new(
            SessionStore::new(dir),
            config,
            auth,
            ModelSelection::new("p", "m"),
        )
        .expect("builder")
        .model_factory(std::sync::Arc::new(move |_, _| {
            Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
                turns.clone(),
            )))
        }))
        .create("C:/w")
        .expect("session")
    }

    fn test_session(tag: &str, turns: Vec<Vec<MockStreamEvent>>) -> Session {
        let dir = test_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);
        build_session(&dir, turns)
    }

    /// A session on a fresh store, for one-off inline tests (the
    /// eof-abort shape test).
    fn test_session_with(
        tag: &str,
        turns: Vec<Vec<MockStreamEvent>>,
        tools: Vec<rig_agent::tool::DynamicTool>,
    ) -> Session {
        let dir = test_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);
        let config = Arc::new(
            TabitConfig::from_toml_str(
                r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"

[[providers.p.models]]
id = "m"
"#,
                Path::new("providers.toml"),
            )
            .expect("config"),
        );
        let auth = Arc::new(AuthConfig::default());
        let mut builder = SessionBuilder::new(
            SessionStore::new(&dir),
            config,
            auth,
            ModelSelection::new("p", "m"),
        )
        .expect("builder")
        .model_factory(std::sync::Arc::new(move |_, _| {
            Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
                turns.clone(),
            )))
        }));
        for tool in tools {
            builder = builder.dynamic_tool(tool);
        }
        builder.create("C:/w").expect("session")
    }

    /// Host wiring over the test store. `create` defaults to a clear
    /// failure (most tests never drive `new_session`); tests that do
    /// pass their own builder.
    fn test_wiring(dir: &Path, create: SessionSource) -> SessionHostWiring {
        SessionHostWiring {
            store: SessionStore::new(dir),
            create,
            open: Arc::new(move |_| Err("open_session is not driven by this test".to_string())),
        }
    }

    fn unusable_create() -> SessionSource {
        Arc::new(|| Err("new_session is not driven by this test".to_string()))
    }

    /// A tool that takes real time, so a run is provably in flight when
    /// the client's input closes.
    fn slow_tool() -> rig_agent::tool::DynamicTool {
        rig_agent::tool::DynamicTool::new(
            "slow",
            "sleeps then echoes",
            serde_json::json!({"type":"object","properties":{"value":{"type":"string"}}}),
            |_ctx, _args| {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    Ok(rig_agent::tool::ToolOutput::text("slept"))
                })
            },
        )
    }

    /// A writer into shared memory, so the test can read what the bridge
    /// wrote.
    #[derive(Clone, Default)]
    struct SharedOut(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for SharedOut {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run the bridge over a fixed input script; returns the exit code
    /// and the parsed server lines. For scripts that never send a
    /// command (the session id is only knowable after the ack).
    async fn bridge(
        tag: &str,
        input: &str,
        turns: Vec<Vec<MockStreamEvent>>,
    ) -> (i32, Vec<ServerFrame>) {
        let session = test_session(tag, turns);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir(tag), unusable_create()),
        );
        let out = SharedOut::default();
        let code = serve(handle, Cursor::new(input.as_bytes().to_vec()), out.clone()).await;
        let written = String::from_utf8(out.0.lock().unwrap().clone()).expect("utf-8");
        let frames = written
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is a frame"))
            .collect();
        (code, frames)
    }

    /// Input whose EOF the test controls: lines arrive through a
    /// channel and the reader stays pending until the sender drops —
    /// the shape of a live client (the fixed-script harness closes
    /// input immediately, which masked the live-forwarding bug).
    struct ChannelIn {
        lines: std::sync::mpsc::Receiver<String>,
        buf: Vec<u8>,
    }

    impl std::io::Read for ChannelIn {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            use std::io::BufRead as _;
            let available = self.fill_buf()?;
            let n = buf.len().min(available.len());
            buf[..n].copy_from_slice(&available[..n]);
            self.consume(n);
            Ok(n)
        }
    }

    impl std::io::BufRead for ChannelIn {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if self.buf.is_empty()
                && let Ok(line) = self.lines.recv()
            {
                self.buf = format!(
                    "{line}
"
                )
                .into_bytes();
            }
            Ok(&self.buf)
        }
        fn consume(&mut self, amt: usize) {
            self.buf.drain(..amt);
        }
    }

    fn read_lines(out: &SharedOut) -> Vec<String> {
        let bytes = out.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The first output line carrying `needle` (polled — the bridge
    /// writes asynchronously).
    async fn await_line(out: &SharedOut, needle: &str) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(line) = read_lines(out).into_iter().find(|l| l.contains(needle)) {
                    return line;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the bridge must produce the awaited line before timeout")
    }

    /// The boot session id, read from the ack line — the honest client
    /// shape v3 forces: commands name their session, and the id is
    /// learnable only from the ack.
    async fn ack_session_id(out: &SharedOut) -> String {
        let ack = await_line(out, "initialize_ack").await;
        match serde_json::from_str::<ServerFrame>(&ack).expect("ack parses") {
            ServerFrame::Control(ServerControlFrame::InitializeAck { session_id, .. }) => {
                session_id
            }
            other => panic!("expected initialize_ack, got {other:?}"),
        }
    }

    /// A `message` wire line for `session`.
    fn message_line(session: &str, text: &str) -> String {
        format!(r#"{{"type":"message","session":"{session}","text":"{text}"}}"#)
    }

    #[tokio::test]
    async fn events_stream_before_the_client_closes_input() {
        // Regression: the bridge forwarded actor events only at
        // wind-down, so a live client saw nothing until it closed
        // stdin (the GUI sat at "queued" while the run streamed).
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let session = test_session("live", vec![script("pong")]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("live"), unusable_create()),
        );
        let out = SharedOut::default();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));

        tx_in.send(r#"{"protocol_version":4}"#.to_string()).unwrap();
        let session_id = ack_session_id(&out).await;
        tx_in.send(message_line(&session_id, "hi")).unwrap();

        // Input stays open: the whole round trip must arrive anyway.
        let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if read_lines(&out).iter().any(|l| l.contains("run_finished")) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            finished.is_ok(),
            "run_finished must arrive while input is open"
        );

        drop(tx_in);
        let code = serve_task.await.unwrap();
        assert_eq!(code, 0);
        let lines = read_lines(&out);
        assert!(lines.iter().any(|l| l.contains(r#""type":"user_message""#)));
        assert!(lines.iter().any(|l| l.contains("pong")));
    }

    fn texts<'a>(frames: &'a [ServerFrame], kind: &str) -> Vec<&'a str> {
        frames
            .iter()
            .filter_map(|frame| match frame {
                ServerFrame::Event(EventFrame {
                    event: tabit_session::SessionEvent::UserMessage { text, .. },
                    ..
                }) if kind == "user" => Some(text.as_str()),
                ServerFrame::Event(EventFrame {
                    event: tabit_session::SessionEvent::TextDelta { text, .. },
                    ..
                }) if kind == "delta" => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Drive the bridge over a live input (a channel the test holds
    /// open): handshake, learn the boot session id from the ack, send
    /// the lines `lines_from` builds for it, drive until `until(output)`
    /// holds, then close — the client shape under the death ruling:
    /// input closing while a run is in flight aborts it, so tests that
    /// want a completed run keep the input open until it finishes.
    async fn bridge_live(
        tag: &str,
        init: &str,
        lines_from: impl Fn(&str) -> Vec<String>,
        turns: Vec<Vec<MockStreamEvent>>,
        until: impl Fn(&[String]) -> bool,
    ) -> (i32, Vec<String>) {
        let session = test_session(tag, turns);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir(tag), unusable_create()),
        );
        let out = SharedOut::default();
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));
        tx_in.send(init.to_string()).unwrap();
        let session_id = ack_session_id(&out).await;
        for line in lines_from(&session_id) {
            tx_in.send(line).unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if until(&read_lines(&out)) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("bridge condition met before timeout");
        drop(tx_in);
        let code = serve_task.await.unwrap();
        (code, read_lines(&out))
    }

    fn parse_frames(lines: &[String]) -> Vec<ServerFrame> {
        lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("every line is a frame"))
            .collect()
    }

    #[tokio::test]
    async fn handshake_then_round_trip_over_memory_buffers() {
        let (code, lines) = bridge_live(
            "roundtrip",
            r#"{"protocol_version":4}"#,
            |session| vec![message_line(session, "hi")],
            vec![script("hello")],
            |lines| lines.iter().any(|l| l.contains("run_finished")),
        )
        .await;
        assert_eq!(code, 0);
        let frames = parse_frames(&lines);
        // The ack comes first and carries the session facts.
        match &frames[0] {
            ServerFrame::Control(ServerControlFrame::InitializeAck {
                protocol_version,
                session_id,
                model,
                ..
            }) => {
                assert_eq!(*protocol_version, tabit_session::PROTOCOL_VERSION);
                assert!(!session_id.is_empty());
                assert_eq!(*model, ModelSelection::new("p", "m"));
            }
            other => panic!("expected initialize_ack, got {other:?}"),
        }
        assert_eq!(texts(&frames, "user"), vec!["hi"]);
        assert_eq!(texts(&frames, "delta"), vec!["hello"]);
        assert!(matches!(
            frames.last(),
            Some(ServerFrame::Event(EventFrame {
                event: tabit_session::SessionEvent::RunFinished { output, .. },
                ..
            })) if output == "hello"
        ));
    }

    #[tokio::test]
    async fn startup_degradations_follow_the_ack_and_never_precede_it() {
        let session = test_session("degraded-startup", vec![script("hello")]);
        let handle = SessionHost::spawn(
            session,
            vec!["the resumed session's model `gone/m` is not usable".to_string()],
            test_wiring(&test_dir("degraded-startup"), unusable_create()),
        );
        let out = SharedOut::default();
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));
        tx_in.send(r#"{"protocol_version":4}"#.to_string()).unwrap();
        let session_id = ack_session_id(&out).await;
        tx_in.send(message_line(&session_id, "hi")).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if read_lines(&out).iter().any(|l| l.contains("run_finished")) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the run must complete before timeout");
        drop(tx_in);
        let code = serve_task.await.unwrap();
        assert_eq!(code, 0);

        let frames = parse_frames(&read_lines(&out));
        // The ack is the first frame; the degradation follows it and
        // precedes every run event — the gate holds even though the
        // worker emitted the note at spawn, before the handshake.
        assert!(matches!(
            frames.first(),
            Some(ServerFrame::Control(
                ServerControlFrame::InitializeAck { .. }
            ))
        ));
        let degraded_at = frames
            .iter()
            .position(|frame| {
                matches!(
                    frame,
                    ServerFrame::Event(EventFrame {
                        event: tabit_session::SessionEvent::Error { kind, .. },
                        ..
                    }) if kind == "model"
                )
            })
            .expect("the degradation frame");
        assert_eq!(degraded_at, 1, "the note lands immediately after the ack");
        let first_user = frames
            .iter()
            .position(|frame| {
                matches!(
                    frame,
                    ServerFrame::Event(EventFrame {
                        event: tabit_session::SessionEvent::UserMessage { .. },
                        ..
                    })
                )
            })
            .expect("a user message");
        assert!(degraded_at < first_user, "the note precedes run events");
    }

    #[tokio::test]
    async fn version_mismatch_rejects_the_connection_and_exits_nonzero() {
        let (code, frames) =
            bridge("mismatch", "{\"protocol_version\":99}\n", vec![script("x")]).await;
        assert_eq!(code, 1);
        assert_eq!(frames.len(), 1, "nothing follows a rejected handshake");
        match &frames[0] {
            ServerFrame::Control(ServerControlFrame::InitializeRejected { reason }) => {
                assert!(reason.contains("required"), "{reason}");
            }
            other => panic!("expected initialize_rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_command_before_initialize_is_a_protocol_error_and_the_connection_survives() {
        // Inline (not bridge_live): the protocol error must precede the
        // handshake, and the command lines need the ack's session id.
        let session = test_session("preinit", vec![script("ok")]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("preinit"), unusable_create()),
        );
        let out = SharedOut::default();
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));
        tx_in
            .send(r#"{"type":"abort","session":"any"}"#.to_string())
            .unwrap();
        await_line(&out, "protocol_error").await;
        tx_in.send(r#"{"protocol_version":4}"#.to_string()).unwrap();
        let session_id = ack_session_id(&out).await;
        tx_in.send(message_line(&session_id, "real")).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if read_lines(&out).iter().any(|l| l.contains("run_finished")) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the run must complete before timeout");
        drop(tx_in);
        let code = serve_task.await.unwrap();
        assert_eq!(code, 0);
        let frames = parse_frames(&read_lines(&out));
        match &frames[0] {
            ServerFrame::Control(ServerControlFrame::ProtocolError { message }) => {
                assert!(message.contains("before initialize"), "{message}");
            }
            other => panic!("expected protocol_error, got {other:?}"),
        }
        // The handshake still succeeds after the error, and the following
        // command runs.
        assert!(matches!(
            &frames[1],
            ServerFrame::Control(ServerControlFrame::InitializeAck { .. })
        ));
        assert_eq!(texts(&frames, "user"), vec!["real"]);
    }

    #[tokio::test]
    async fn input_closing_mid_run_aborts_it_instead_of_draining() {
        // The death ruling at the stdio edge: EOF while a run is in
        // flight aborts it (the core dies with the frontend) — a slow
        // run must not keep the process alive for a client that is
        // gone. The abort terminal still flushes before the exit.
        let slow = vec![
            MockStreamEvent::text("partial"),
            MockStreamEvent::tool_call("t1", "slow", serde_json::json!({})),
            MockStreamEvent::final_response(Usage::default()),
        ];
        let finish = vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response(Usage::default()),
        ];
        let session = test_session_with("eof-abort", vec![slow, finish], vec![slow_tool()]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("eof-abort"), unusable_create()),
        );
        let out = SharedOut::default();
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));

        tx_in.send(r#"{"protocol_version":4}"#.to_string()).unwrap();
        let session_id = ack_session_id(&out).await;
        tx_in.send(message_line(&session_id, "slow one")).unwrap();

        // Close the input the moment the tool call is provably in
        // flight (the tool sleeps, so the window is wide).
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if read_lines(&out)
                    .iter()
                    .any(|l| l.contains(r#""type":"tool_call""#))
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the tool call must arrive");
        drop(tx_in);

        let code = tokio::time::timeout(std::time::Duration::from_secs(5), serve_task)
            .await
            .expect("the bridge must exit after the abort")
            .unwrap();
        assert_eq!(code, 0);
        let lines = read_lines(&out);
        assert!(
            lines.iter().any(|l| l.contains("run_aborted")),
            "EOF mid-run must abort the run, got: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("run_finished")),
            "the unattended run must not complete"
        );
    }

    #[tokio::test]
    async fn an_unparseable_line_is_reported_and_skipped() {
        // A blank line between frames is skipped, and a second
        // initialize is a protocol error, not a restart.
        let (code, frames) = bridge(
            "garbage",
            "this is not json\n\n{\"protocol_version\":4}\n{\"protocol_version\":4}\n",
            vec![script("x")],
        )
        .await;
        assert_eq!(code, 0);
        match &frames[0] {
            ServerFrame::Control(ServerControlFrame::ProtocolError { message }) => {
                assert!(message.contains("unparseable"), "{message}");
            }
            other => panic!("expected protocol_error, got {other:?}"),
        }
        assert!(matches!(
            &frames[1],
            ServerFrame::Control(ServerControlFrame::InitializeAck { .. })
        ));
        match &frames[2] {
            ServerFrame::Control(ServerControlFrame::ProtocolError { message }) => {
                assert!(message.contains("already initialized"), "{message}");
            }
            other => panic!("expected protocol_error, got {other:?}"),
        }
    }

    /// A reader that panics on first use — the bridge treats a dead
    /// reader thread as a broken edge (exit 1).
    struct PanickingReader;

    impl Read for PanickingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!("reader died");
        }
    }

    impl BufRead for PanickingReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            panic!("reader died");
        }
        fn consume(&mut self, _amt: usize) {}
    }

    /// A reader whose reads error out — the bridge treats it like EOF
    /// (clean wind-down, exit 0).
    struct ErroringReader;

    impl Read for ErroringReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("pipe gone"))
        }
    }

    impl BufRead for ErroringReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Err(std::io::Error::other("pipe gone"))
        }
        fn consume(&mut self, _amt: usize) {}
    }

    /// A writer that refuses everything — the writer loop stops, the
    /// bridge still winds down cleanly (exit 0).
    #[derive(Default)]
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("client gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("client gone"))
        }
    }

    #[tokio::test]
    async fn broken_transport_edges_fail_or_end_cleanly() {
        // Panicking reader: broken edge, exit 1.
        let session = test_session("panic-reader", vec![script("x")]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("panic-reader"), unusable_create()),
        );
        let code = serve(handle, PanickingReader, SharedOut::default()).await;
        assert_eq!(code, 1);

        // Erroring reader: reads as EOF, exit 0.
        let session = test_session("error-reader", vec![script("x")]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("error-reader"), unusable_create()),
        );
        let code = serve(handle, ErroringReader, SharedOut::default()).await;
        assert_eq!(code, 0);

        // Failing writer: the writer stops, the rest winds down, exit 0.
        let session = test_session("fail-writer", vec![script("x")]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("fail-writer"), unusable_create()),
        );
        let code = serve(
            handle,
            Cursor::new(b"{\"protocol_version\":4}\n".to_vec()),
            FailingWriter,
        )
        .await;
        assert_eq!(code, 0);
    }
    #[tokio::test]
    async fn initialize_with_replay_receives_the_pass_right_after_the_ack() {
        // A session with history (run once, then resumed through the
        // same store): the handshake's `replay: true` streams the pass
        // after the ack, whole-text, bracketed — and a message
        // afterwards runs normally.
        let dir = std::env::temp_dir()
            .join("tabit-json-tests")
            .join(format!("replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = tabit_session::SessionStore::new(&dir);
        let history_session = |answer: &'static str, store: tabit_session::SessionStore| {
            let config = Arc::new(
                TabitConfig::from_toml_str(
                    r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"

[[providers.p.models]]
id = "m"
"#,
                    Path::new("providers.toml"),
                )
                .expect("config"),
            );
            SessionBuilder::new(
                store,
                config,
                Arc::new(AuthConfig::default()),
                ModelSelection::new("p", "m"),
            )
            .expect("builder")
            .model_factory(Arc::new(move |_, _| {
                Ok(ModelHandle::new(MockCompletionModel::from_stream_turns([
                    [
                        MockStreamEvent::text(answer),
                        MockStreamEvent::final_response_with_default_usage(),
                    ],
                ])))
            }))
        };
        let path = {
            let mut session = history_session("first answer", store.clone())
                .create("C:/w")
                .unwrap();
            session.prompt("hello").await;
            session.path().to_path_buf()
        };
        let session = history_session("second answer", store)
            .resume(&path)
            .unwrap()
            .0;
        let handle = SessionHost::spawn(session, Vec::new(), test_wiring(&dir, unusable_create()));

        let out = SharedOut::default();
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));
        tx_in
            .send(r#"{"protocol_version":4,"replay":true}"#.to_string())
            .unwrap();
        let session_id = ack_session_id(&out).await;
        tx_in.send(message_line(&session_id, "again")).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if read_lines(&out).iter().any(|l| l.contains("run_finished")) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the follow-up run must complete");
        drop(tx_in);
        assert_eq!(serve_task.await.unwrap(), 0);

        let frames = parse_frames(&read_lines(&out));
        assert!(matches!(
            frames.first(),
            Some(ServerFrame::Control(
                ServerControlFrame::InitializeAck { .. }
            ))
        ));
        // The v3 startup announcement: the catalog lands between the
        // ack's control frame and the replay pass — and it lists the
        // resumed session itself.
        let catalog_at = frames
            .iter()
            .position(|frame| {
                matches!(
                    frame,
                    ServerFrame::Event(EventFrame {
                        event: tabit_session::SessionEvent::SessionsAvailable { sessions },
                        ..
                    }) if sessions.iter().any(|s| s.id == session_id)
                )
            })
            .expect("sessions_available listing the boot session");
        assert_eq!(catalog_at, 1, "the catalog is the first event frame");
        let types: Vec<&str> = frames
            .iter()
            .filter_map(|frame| match frame {
                ServerFrame::Event(event) => match &event.event {
                    tabit_session::SessionEvent::ReplayStarted { .. } => Some("replay_started"),
                    tabit_session::SessionEvent::ReplayDone => Some("replay_done"),
                    tabit_session::SessionEvent::ModelChanged { .. } => Some("model_changed"),
                    tabit_session::SessionEvent::UserMessage { .. } => Some("user_message"),
                    tabit_session::SessionEvent::TurnStarted { .. } => Some("turn_started"),
                    tabit_session::SessionEvent::TextDelta { .. } => Some("text_delta"),
                    tabit_session::SessionEvent::CompletionCall { .. } => Some("completion_call"),
                    tabit_session::SessionEvent::TurnCommitted { .. } => Some("turn_committed"),
                    tabit_session::SessionEvent::RunFinished { .. } => Some("run_finished"),
                    tabit_session::SessionEvent::SessionsAvailable { .. } => {
                        Some("sessions_available")
                    }
                    _ => Some("other"),
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            types,
            vec![
                "sessions_available",
                "replay_started",
                "model_changed",
                "user_message",
                "turn_started",
                "text_delta",
                "completion_call",
                "turn_committed",
                "replay_done",
                // The follow-up message runs after the pass, live.
                "user_message",
                "turn_started",
                "text_delta",
                "completion_call",
                "turn_committed",
                "run_finished",
            ]
        );
        // History arrives whole: one delta carrying the full first
        // answer.
        assert!(
            frames.iter().any(|frame| matches!(frame,
                ServerFrame::Event(EventFrame {
                    event: tabit_session::SessionEvent::TextDelta { text, .. },
                    ..
                }) if text == "first answer"
            )),
            "history arrives whole"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn initialize_without_replay_gets_no_pass() {
        let (code, frames) = bridge(
            "no-replay",
            r#"{"protocol_version":4}"#,
            vec![script("hello")],
        )
        .await;
        assert_eq!(code, 0);
        assert!(
            !frames.iter().any(|frame| matches!(
                frame,
                ServerFrame::Event(EventFrame {
                    event: tabit_session::SessionEvent::ReplayStarted { .. },
                    ..
                })
            )),
            "no replay request, no pass"
        );
    }

    #[tokio::test]
    async fn new_session_and_open_session_serve_many_sessions_on_one_connection() {
        // The v3 host over the wire: the catalog arrives at startup;
        // `new_session` announces a fresh session (its events stamp a
        // second stream); `open_session` re-replays the boot session on
        // request — one connection, two sessions, attribution by stamp.
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let dir = test_dir("multi");
        let _ = std::fs::remove_dir_all(&dir);
        let session = build_session(&dir, vec![script("boot answer")]);
        let created_dir = dir.clone();
        let create: SessionSource = Arc::new(move || {
            Ok((
                build_session(&created_dir, vec![script("new answer")]),
                Vec::new(),
            ))
        });
        let open_store = SessionStore::new(&dir);
        let open: tabit_session::OpenSessionSource = Arc::new(move |id: &str| {
            let summary = open_store
                .list()
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|summary| summary.id == id)
                .ok_or_else(|| format!("no stored session with id `{id}`"))?;
            let config = Arc::new(
                TabitConfig::from_toml_str(
                    r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"

[[providers.p.models]]
id = "m"
"#,
                    Path::new("providers.toml"),
                )
                .expect("config"),
            );
            let store = open_store.clone();
            let factory_config = config.clone();
            SessionBuilder::new(
                store,
                factory_config,
                Arc::new(AuthConfig::default()),
                ModelSelection::new("p", "m"),
            )
            .expect("builder")
            .model_factory(Arc::new(move |_, _| {
                Ok(ModelHandle::new(MockCompletionModel::from_stream_turns([
                    [
                        MockStreamEvent::text("reopened answer"),
                        MockStreamEvent::final_response_with_default_usage(),
                    ],
                ])))
            }))
            .resume(&summary.path)
            .map(|(session, _)| (session, Vec::new()))
            .map_err(|e| e.to_string())
        });
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            SessionHostWiring {
                store: SessionStore::new(&dir),
                create,
                open,
            },
        );
        let out = SharedOut::default();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));

        // Handshake, then one live round trip so the boot session
        // materializes on disk (the catalog lists stored files only).
        tx_in.send(r#"{"protocol_version":4}"#.to_string()).unwrap();
        let boot = ack_session_id(&out).await;
        tx_in.send(message_line(&boot, "hello")).unwrap();
        await_line(&out, "run_finished").await;

        // A second session, by command: session_created carries its id
        // and stamps its stream.
        tx_in.send(r#"{"type":"new_session"}"#.to_string()).unwrap();
        let created_line = await_line(&out, "session_created").await;
        let created = match serde_json::from_str::<ServerFrame>(&created_line).expect("frame") {
            ServerFrame::Event(EventFrame {
                event: tabit_session::SessionEvent::SessionCreated { id, .. },
                ..
            }) => id,
            other => panic!("expected session_created, got {other:?}"),
        };
        assert_ne!(created, boot, "the new session is a second stream");
        // Backend-level: no stream field on the creation line; the
    // payload names the session (the optional-stream ruling — this
    // was the assertion that once pinned the faked stamp).
    assert!(!created_line.contains("stream"));
    assert!(created_line.contains(&format!(r#""id":"{created}""#)));;

        // The new session answers a message on its own stamp — the
        // await is stream-scoped so a stale terminal line from another
        // session can never satisfy it early.
        tx_in.send(message_line(&created, "hi again")).unwrap();
        let answer_line = await_line(&out, "new answer").await;
        assert!(answer_line.contains(&format!(r#""stream":"{created}""#)));
        await_line(&out, "run_finished").await;

        // open_session of the boot session: an idempotent re-replay of
        // its (now stored) chain, stamped with the boot id.
        tx_in
            .send(format!(r#"{{"type":"open_session","id":"{boot}"}}"#))
            .unwrap();
        await_line(&out, "replay_done").await;
        let replay_started = await_line(&out, "replay_started").await;
        assert!(replay_started.contains(&format!(r#""stream":"{boot}""#)));

        drop(tx_in);
        assert_eq!(serve_task.await.unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn input_closing_burst_close_never_runs_unattended_messages() {
        // The death door is dropping the host, not the polite close: a
        // message queued but unrouted at EOF must die unrouted (the
        // polite close's drain would route it — starting an
        // unattended run that spends model calls; the review round's
        // finding). The slow tool makes "routed then aborted" and
        // "never routed" both finish-free, so the assertion is
        // deterministic either way.
        let slow = vec![
            MockStreamEvent::text("partial"),
            MockStreamEvent::tool_call("t1", "slow", serde_json::json!({})),
            MockStreamEvent::final_response(Usage::default()),
        ];
        let session = test_session_with("eof-burst", vec![slow], vec![slow_tool()]);
        let dir = test_dir("eof-burst");
        let handle = SessionHost::spawn(session, Vec::new(), test_wiring(&dir, unusable_create()));
        let out = SharedOut::default();
        // The piped-burst shape (initialize + message + immediate EOF)
        // needs the session id, which is only knowable after the ack —
        // so drive it live: handshake, learn the id, send the message,
        // and close input immediately.
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));
        tx_in.send(r#"{"protocol_version":4}"#.to_string()).unwrap();
        let session_id = ack_session_id(&out).await;
        tx_in.send(message_line(&session_id, "hi")).unwrap();
        drop(tx_in);

        let code = tokio::time::timeout(std::time::Duration::from_secs(5), serve_task)
            .await
            .expect("the bridge must exit after EOF")
            .unwrap();
        assert_eq!(code, 0);
        let lines = read_lines(&out);
        assert!(
            !lines.iter().any(|l| l.contains("run_finished")),
            "no unattended run completes: {lines:?}"
        );
        // (A message routed BEFORE the EOF legitimately entered
        // history and aborted mid-run — that is the ruled behavior;
        // what the death door forbids is completing runs for the
        // gone, and routing what never got there.)
    }

    #[tokio::test]
    async fn a_message_for_an_unknown_session_is_a_session_error() {
        let (code, lines) = bridge_live(
            "unknown-session",
            r#"{"protocol_version":4}"#,
            |session| {
                vec![
                    message_line("no-such-session", "lost?"),
                    // The boot session still works afterwards.
                    message_line(session, "still here"),
                ]
            },
            vec![script("fine")],
            |lines| {
                lines.iter().any(|l| l.contains("run_finished"))
                    && lines.iter().any(|l| l.contains("\"kind\":\"session\""))
            },
        )
        .await;
        assert_eq!(code, 0);
        let error_line = lines
            .iter()
            .find(|l| l.contains("\"kind\":\"session\""))
            .expect("the session error");
        // Routing errors are backend-level (the optional-stream
        // ruling): no faked stamp; the message names the id.
        assert!(
            !error_line.contains("stream"),
            "the routing error carries no stamp: {error_line}"
        );
        assert!(
            error_line.contains("no-such-session"),
            "the message names the target: {error_line}"
        );
        assert!(lines.iter().any(|l| l.contains("still here")));
    }

    #[tokio::test]
    async fn a_checkout_round_trips_the_wire() {
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let session = test_session("checkout", vec![script("first"), script("second")]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("checkout"), unusable_create()),
        );
        let out = SharedOut::default();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));

        tx_in.send(r#"{"protocol_version":4}"#.to_string()).unwrap();
        let session_id = ack_session_id(&out).await;
        tx_in.send(message_line(&session_id, "hi")).unwrap();
        await_line(&out, "run_finished").await;

        // The checkout target is learnable only from the user_message
        // line — the honest client shape (entry ids come from events).
        let entry_id = read_lines(&out)
            .into_iter()
            .find_map(|line| match serde_json::from_str::<ServerFrame>(&line) {
                Ok(ServerFrame::Event(EventFrame {
                    event: tabit_session::SessionEvent::UserMessage { text, entry_id },
                    ..
                })) if text == "hi" => Some(entry_id),
                _ => None,
            })
            .expect("the user_message line carries the entry id");

        tx_in
            .send(format!(
                r#"{{"type":"checkout","session":"{session_id}","entry_id":"{entry_id}"}}"#
            ))
            .unwrap();
        let checked = await_line(&out, "checked_out").await;
        assert!(
            checked.contains(&format!(r#""entry_id":"{entry_id}""#)),
            "the target echoes back: {checked}"
        );
        assert!(
            checked.contains(r#""base_id":null"#),
            "full re-render rides an explicit null: {checked}"
        );
        await_line(&out, "replay_done").await;

        // The session is fully alive after the rewind: the next prompt
        // branches and runs.
        tx_in.send(message_line(&session_id, "again")).unwrap();
        await_line(&out, "second").await;
        drop(tx_in);
        let code = serve_task.await.unwrap();
        assert_eq!(code, 0);
        std::fs::remove_dir_all(test_dir("checkout")).ok();
    }
}
