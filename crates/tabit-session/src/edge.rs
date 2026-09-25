//! The serve side of the frozen wire: the stdio edge of the session
//! protocol (JSON mode). LF-JSONL in both directions — the client's
//! lines are bare [`SessionCommand`]s (the report model: no
//! handshake — commands may flow from the spawner's first line), the
//! server's lines are its self-report, stamped [`EventFrame`]s, plus
//! transport-error control frames. Only protocol bytes reach
//! stdout; human banners stay on stderr.
//!
//! This is the wire's server half living with the host it drives (the
//! extraction ruling 2026-09: the client half and the child substrate
//! are `tabit-wire`; one server exists, so it lives here, out of the
//! binary). The whole bridge is generic over `BufRead`/`Write` so
//! tests drive it with in-memory buffers instead of process pipes.

use std::io::{BufRead, Write};
use tabit_protocol::EventFrame;
use tabit_protocol::{PROTOCOL_VERSION, ServerControlFrame, ServerFrame, SessionCommand};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::endpoint::SessionHost;

/// Serve the backend over `reader`/`writer` until the client closes its
/// input (or the stream behind the edge ends). Returns the process
/// exit code: 0 normally, 1 on a broken reader.
pub async fn serve<R, W>(mut host: SessionHost, reader: R, mut writer: W) -> i32
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    // The self-report — this child's FIRST line on the channel (owner
    // ruling 2026-09-25, the report model: a spawned child can assume
    // its spawner is there and pump, while the spawner can assume
    // nothing until the child self-reports) — is written HERE,
    // synchronously, before any task exists: nothing can precede it
    // because nothing else has started. Protocol facts only; the
    // spawner version-checks and kills an incompatible child, and
    // every session fact arrives by event (the boot's
    // `session_opened` first). A failing write is the client already
    // gone — the wind-down below is the same either way.
    let report = ServerFrame::Control(ServerControlFrame::Report {
        protocol_version: PROTOCOL_VERSION,
    });
    let _ = writeln!(writer, "{}", tabit_protocol::to_wire_line(&report));
    let _ = writer.flush();

    // One feed into the writer — the reader's protocol errors and the
    // forwarder's events, in send order. The writer ends on the
    // stream's end signal with a drain, never on the feed's close:
    // the reader thread parks in a blocking read that nothing on this
    // side can interrupt, and its sender clone would hold the feed
    // open forever on the stream-end path (the constraint that once
    // forced a second, prioritized feed and a closed-arm latch — the
    // end token retires both).
    let (feed_tx, feed_rx) = mpsc::unbounded_channel::<ServerFrame>();

    let dispatch = {
        let link = host.command_link();
        std::sync::Arc::new(move |command: SessionCommand| link.send(command))
    };
    let reader_feed = feed_tx.clone();
    let reader_task = tokio::task::spawn_blocking(move || read_loop(reader, dispatch, reader_feed));
    let writer_task = tokio::spawn(write_loop(feed_rx, writer, host.stream_end_signal()));

    // The live forwarder: host events reach stdout as they happen, for
    // the whole connection — not only at wind-down. (v1 bug: events
    // accumulated unread in the actor's channel until the client
    // closed stdin; the GUI sat at "queued" forever while the run was
    // already streaming.) The stream's end signal fires after every
    // worker's last event has landed (the host's wind-down awaits the
    // joins first), so draining what remains and stopping is lossless.
    let events = host.take_events();
    let end = host.stream_end_signal();
    let forwarder_task = tokio::spawn(forward_events(events, feed_tx, end.clone()));

    // A panicked reader thread is a broken edge: exit nonzero. The
    // stream's end is the other way out: when the host behind the
    // edge is done (its wind-down fired), waiting on a client that
    // may never close stdin would hang the connection open-and-quiet
    // forever — the end resolves the edge instead, after the
    // forwarder's own end arm drained what landed.
    let exit = tokio::select! {
        biased;
        code = reader_task => code.unwrap_or(1),
        _ = end.cancelled() => 0,
    };
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
    // The forwarder ends when the stream ends; the writer follows it
    // out after writing everything it was fed.
    let _ = forwarder_task.await;
    let _ = writer_task.await;
    exit
}

/// Pump the actor's event stream into the writer feed until the
/// stream ends — nothing follows the host's wind-down signal (fired
/// only after every worker's last event landed).
async fn forward_events(
    stream: Option<mpsc::UnboundedReceiver<EventFrame>>,
    out: mpsc::UnboundedSender<ServerFrame>,
    end: CancellationToken,
) {
    let Some(mut stream) = stream else {
        return;
    };
    loop {
        tokio::select! {
            frame = stream.recv() => {
                let Some(frame) = frame else { return };
                // Frames cross verbatim, hop budget included — the
                // consumer ignores what it doesn't know (watching
                // extension lanes hear the same frames through their
                // own node subscriptions, not through this writer).
                let _ = out.send(ServerFrame::Event(frame));
            }
            _ = end.cancelled() => {
                // The wind-down awaited every worker join before firing
                // this: drain what landed, then stop.
                while let Ok(frame) = stream.try_recv() {
                    let _ = out.send(ServerFrame::Event(frame));
                }
                return;
            }
        }
    }
}

/// Parse client lines into commands and dispatch them until EOF.
/// Blocking, runs on its own thread. Under the report model there is
/// no handshake state: commands may flow from the spawner's very
/// first line — before or after this child's report — and the pipe
/// buffers whatever arrives early.
fn read_loop<R: BufRead>(
    mut reader: R,
    dispatch: std::sync::Arc<dyn Fn(SessionCommand) + Send + Sync>,
    out: mpsc::UnboundedSender<ServerFrame>,
) -> i32 {
    fn control(out: &mpsc::UnboundedSender<ServerFrame>, frame: ServerControlFrame) {
        let _ = out.send(ServerFrame::Control(frame));
    }

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
        match serde_json::from_str::<SessionCommand>(frame) {
            Ok(command) => dispatch(command),
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

/// Serialize frames one per line — the single owner of stdout,
/// draining the one feed until the stream's end. The end signal
/// fires after every worker's last event has landed (the wind-down
/// awaits the joins first), so the drain that follows it is
/// lossless; the loop never WAITS on the feed's close, for the
/// reader thread's parked read holds its sender clone (the
/// stream-end path's constraint). A failing write means the client
/// is gone: stop, the shutdown path ends everything else.
async fn write_loop<W: Write>(
    mut feed: mpsc::UnboundedReceiver<ServerFrame>,
    mut writer: W,
    end: CancellationToken,
) {
    loop {
        tokio::select! {
            frame = feed.recv() => {
                let Some(frame) = frame else { break };
                let line = tabit_protocol::to_wire_line(&frame);
                if writeln!(writer, "{line}").is_err() {
                    break;
                }
            }
            _ = end.cancelled() => {
                while let Ok(frame) = feed.try_recv() {
                    let line = tabit_protocol::to_wire_line(&frame);
                    if writeln!(writer, "{line}").is_err() {
                        return;
                    }
                }
                break;
            }
        }
    }
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::SessionHostData;
    use crate::{
        EventFrame, ModelSelection, Session, SessionBuilder, SessionHost, SessionHostWiring,
        SessionSource, SessionStore,
    };
    use rig_agent::agent::ModelHandle;
    use rig_agent::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig_core::completion::Usage;
    use std::io::{Cursor, Read};
    use std::path::Path;
    use std::sync::Arc;
    use tabit_config::{AuthConfig, TabitConfig};

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
keyless = true

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
        .model_factory(std::sync::Arc::new(move |_, _, _| {
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
keyless = true

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
        .model_factory(std::sync::Arc::new(move |_, _, _| {
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
    fn test_wiring(dir: &Path) -> SessionHostWiring {
        SessionHostWiring {
            node: std::sync::Arc::new(crate::Node::new("test")),
            boot_parent: None,
            boot_parent_call: None,
            store: SessionStore::new(dir),
        }
    }

    /// The shared refusing builders with one `create` override.
    fn test_data(create: SessionSource) -> SessionHostData {
        SessionHostData {
            create,
            ..crate::tests::plain_data()
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
    /// command (the session id is only knowable after the announce).
    async fn bridge(
        tag: &str,
        input: &str,
        turns: Vec<Vec<MockStreamEvent>>,
    ) -> (i32, Vec<ServerFrame>) {
        let session = test_session(tag, turns);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir(tag)),
            test_data(unusable_create()),
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

    /// The boot session id, read from the boot's `session_opened` —
    /// the honest client shape the report model forces: commands name
    /// their session, and the id is learnable only from the announce.
    async fn boot_session_id(out: &SharedOut) -> String {
        let opened = await_line(out, "session_opened").await;
        match serde_json::from_str::<ServerFrame>(&opened).expect("announce parses") {
            ServerFrame::Event(EventFrame {
                event: crate::SessionEvent::SessionOpened { id, .. },
                ..
            }) => id,
            other => panic!("expected session_opened, got {other:?}"),
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
            test_wiring(&test_dir("live")),
            test_data(unusable_create()),
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

        let session_id = boot_session_id(&out).await;
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
                    origin: None,
                    event: crate::SessionEvent::UserMessage { text, .. },
                    ..
                }) if kind == "user" => Some(text.as_str()),
                ServerFrame::Event(EventFrame {
                    origin: None,
                    event: crate::SessionEvent::TextDelta { text, .. },
                    ..
                }) if kind == "delta" => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Drive the bridge over a live input (a channel the test holds
    /// open): learn the boot session id from the announce, send
    /// the lines `lines_from` builds for it, drive until `until(output)`
    /// holds, then close — the client shape under the death ruling:
    /// input closing while a run is in flight aborts it, so tests that
    /// want a completed run keep the input open until it finishes.
    async fn bridge_live(
        tag: &str,
        lines_from: impl Fn(&str) -> Vec<String>,
        turns: Vec<Vec<MockStreamEvent>>,
        until: impl Fn(&[String]) -> bool,
    ) -> (i32, Vec<String>) {
        let session = test_session(tag, turns);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir(tag)),
            test_data(unusable_create()),
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
        let session_id = boot_session_id(&out).await;
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
            |session| vec![message_line(session, "hi")],
            vec![script("hello")],
            |lines| lines.iter().any(|l| l.contains("run_finished")),
        )
        .await;
        assert_eq!(code, 0);
        let frames = parse_frames(&lines);
        // The report is the first line and carries protocol facts only
        // — the session's facts arrive as the session_opened event.
        match &frames[0] {
            ServerFrame::Control(ServerControlFrame::Report { protocol_version }) => {
                assert_eq!(*protocol_version, crate::PROTOCOL_VERSION);
            }
            other => panic!("expected the report, got {other:?}"),
        }
        let boot_id = match &frames[1] {
            ServerFrame::Event(EventFrame {
                origin: None,
                event: crate::SessionEvent::SessionOpened { id, .. },
                ..
            }) => id.clone(),
            other => panic!("expected session_opened second, got {other:?}"),
        };
        let opened = frames.iter().find_map(|frame| match frame {
            ServerFrame::Event(EventFrame {
                origin: None,
                event: crate::SessionEvent::SessionOpened { id, model, .. },
                ..
            }) => Some((id.clone(), model.clone())),
            _ => None,
        });
        let (opened_id, opened_model) =
            opened.expect("the boot session announces itself by session_opened");
        assert_eq!(opened_id, boot_id);
        assert_eq!(opened_model, ModelSelection::new("p", "m"));
        assert_eq!(texts(&frames, "user"), vec!["hi"]);
        assert_eq!(texts(&frames, "delta"), vec!["hello"]);
        assert!(matches!(
            frames.last(),
            Some(ServerFrame::Event(EventFrame {
                origin: None,
                event: crate::SessionEvent::RunFinished { output, .. },
                ..
            })) if output == "hello"
        ));
    }

    #[tokio::test]
    async fn startup_degradations_follow_the_report_and_never_precede_it() {
        let session = test_session("degraded-startup", vec![script("hello")]);
        let handle = SessionHost::spawn(
            session,
            vec!["the resumed session's model `gone/m` is not usable".to_string()],
            test_wiring(&test_dir("degraded-startup")),
            test_data(unusable_create()),
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
        let session_id = boot_session_id(&out).await;
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
        // The report is the first line; session_opened and the
        // degradation follow it and precede every run event — the
        // synchronous report write holds even though the worker
        // emitted the note at spawn, before the edge served.
        assert!(matches!(
            frames.first(),
            Some(ServerFrame::Control(ServerControlFrame::Report { .. }))
        ));
        let degraded_at = frames
            .iter()
            .position(|frame| {
                matches!(
                    frame,
                    ServerFrame::Event(EventFrame {
                        origin: None,
                        event: crate::SessionEvent::Error { kind, .. },
                        ..
                    }) if kind == "model"
                )
            })
            .expect("the degradation frame");
        assert_eq!(
            degraded_at, 2,
            "session_opened, then the note, both ahead of any run event"
        );
        let first_user = frames
            .iter()
            .position(|frame| {
                matches!(
                    frame,
                    ServerFrame::Event(EventFrame {
                        origin: None,
                        event: crate::SessionEvent::UserMessage { .. },
                        ..
                    })
                )
            })
            .expect("a user message");
        assert!(degraded_at < first_user, "the note precedes run events");
    }

    #[tokio::test]
    async fn a_command_on_the_first_line_dispatches() {
        // The report model: commands may flow from the spawner's very
        // first line — before the child's own report is even written.
        // The abort names an unknown session: its outcome is the
        // uniform unstamped error, proving the dispatch ran.
        let session = test_session("preinit", vec![script("ok")]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("preinit")),
            test_data(unusable_create()),
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
        await_line(&out, "no session `any` on this node").await;
        let session_id = boot_session_id(&out).await;
        tx_in.send(message_line(&session_id, "real")).unwrap();
        await_line(&out, "run_finished").await;
        drop(tx_in);
        assert_eq!(serve_task.await.unwrap(), 0);
        let _ = std::fs::remove_dir_all(test_dir("preinit"));
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
            test_wiring(&test_dir("eof-abort")),
            test_data(unusable_create()),
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

        let session_id = boot_session_id(&out).await;
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
        // A blank line between frames is skipped; every unparseable
        // line is a protocol error, and the connection survives them.
        let (code, frames) = bridge(
            "garbage",
            "this is not json\n\nalso not json\n",
            vec![script("x")],
        )
        .await;
        assert_eq!(code, 0);
        // The report is always first.
        assert!(matches!(
            &frames[0],
            ServerFrame::Control(ServerControlFrame::Report { .. })
        ));
        // Past the report the ordering is genuinely concurrent: the
        // reader's protocol_error races the host's session_opened
        // through the forwarder. Assert the multiset, not positions.
        let protocol_errors: Vec<&str> = frames
            .iter()
            .filter_map(|frame| match frame {
                ServerFrame::Control(ServerControlFrame::ProtocolError { message }) => {
                    Some(message.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(protocol_errors.len(), 2, "{frames:?}");
        assert!(
            protocol_errors.iter().all(|m| m.contains("unparseable")),
            "{protocol_errors:?}"
        );
        assert!(
            frames.iter().any(|frame| matches!(
                frame,
                ServerFrame::Event(EventFrame {
                    origin: None,
                    event: crate::SessionEvent::SessionOpened { .. },
                    ..
                })
            )),
            "the boot's session_opened arrives too: {frames:?}"
        );
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
            test_wiring(&test_dir("panic-reader")),
            test_data(unusable_create()),
        );
        let code = serve(handle, PanickingReader, SharedOut::default()).await;
        assert_eq!(code, 1);

        // Erroring reader: reads as EOF, exit 0.
        let session = test_session("error-reader", vec![script("x")]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("error-reader")),
            test_data(unusable_create()),
        );
        let code = serve(handle, ErroringReader, SharedOut::default()).await;
        assert_eq!(code, 0);

        // Failing writer: the writer stops, the rest winds down, exit 0.
        let session = test_session("fail-writer", vec![script("x")]);
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("fail-writer")),
            test_data(unusable_create()),
        );
        let code = serve(handle, Cursor::new(Vec::new()), FailingWriter).await;
        assert_eq!(code, 0);
    }
    #[tokio::test]
    async fn a_resumed_boot_replays_automatically() {
        // A session with history (run once, then resumed through the
        // same store): the boot replays by default (owner ruling
        // 2026-09-25) — the pass streams whole-text, bracketed, right
        // after the startup announcements — and a message afterwards
        // runs normally. No request needed; a fresh boot replays
        // nothing (its own test below).
        let dir = std::env::temp_dir()
            .join("tabit-json-tests")
            .join(format!("replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = crate::SessionStore::new(&dir);
        let history_session = |answer: &'static str, store: crate::SessionStore| {
            let config = Arc::new(
                TabitConfig::from_toml_str(
                    r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"
keyless = true

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
            .model_factory(Arc::new(move |_, _, _| {
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
            session.path().expect("file-backed").to_path_buf()
        };
        let session = history_session("second answer", store)
            .resume(&path)
            .unwrap()
            .0;
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&dir),
            test_data(unusable_create()),
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
        let session_id = boot_session_id(&out).await;
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
            Some(ServerFrame::Control(ServerControlFrame::Report { .. }))
        ));
        // The v3 startup announcement: the boot's session_opened leads
        // (every session becoming visible is announced the same way),
        // then the catalog — listing the resumed session itself —
        // lands between it and the replay pass.
        let catalog_at = frames
            .iter()
            .position(|frame| {
                matches!(
                    frame,
                    ServerFrame::Event(EventFrame {
                        origin: None,
                        event: crate::SessionEvent::SessionsAvailable { sessions },
                        ..
                    }) if sessions.iter().any(|s| s.id == session_id)
                )
            })
            .expect("sessions_available listing the boot session");
        // v16: the announce and the catalog rows both carry the
        // session's cwd (and the rows its file path) — a frontend
        // never lists the directory to learn either.
        let opened = frames.iter().find_map(|frame| match frame {
            ServerFrame::Event(EventFrame {
                origin: None,
                event: crate::SessionEvent::SessionOpened { cwd, path, .. },
                ..
            }) => Some((cwd.clone(), path.clone())),
            _ => None,
        });
        let (opened_cwd, _opened_path) = opened.expect("session_opened on the wire");
        assert!(!opened_cwd.is_empty(), "the boot announces its cwd");
        if let ServerFrame::Event(EventFrame {
            origin: None,
            event: crate::SessionEvent::SessionsAvailable { sessions },
            ..
        }) = &frames[catalog_at]
        {
            for row in sessions {
                assert!(!row.cwd.is_empty(), "catalog rows carry cwd");
                assert!(!row.path.is_empty(), "catalog rows carry the file path");
            }
        }
        assert_eq!(
            catalog_at, 2,
            "session_opened, then the catalog, then the pass"
        );
        let types: Vec<&str> = frames
            .iter()
            .filter_map(|frame| match frame {
                ServerFrame::Event(event) => match &event.event {
                    crate::SessionEvent::ReplayBegin { .. } => Some("replay_begin"),
                    crate::SessionEvent::ReplayEnd => Some("replay_end"),
                    crate::SessionEvent::ModelChanged { .. } => Some("model_changed"),
                    crate::SessionEvent::UserMessage { .. } => Some("user_message"),
                    crate::SessionEvent::TurnStarted { .. } => Some("turn_started"),
                    crate::SessionEvent::TextDelta { .. } => Some("text_delta"),
                    crate::SessionEvent::CompletionCall { .. } => Some("completion_call"),
                    crate::SessionEvent::TurnCommitted { .. } => Some("turn_committed"),
                    crate::SessionEvent::RunFinished { .. } => Some("run_finished"),
                    crate::SessionEvent::SessionsAvailable { .. } => Some("sessions_available"),
                    crate::SessionEvent::SessionOpened { .. } => Some("session_opened"),
                    _ => Some("other"),
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            types,
            vec![
                "session_opened",
                "sessions_available",
                // The register announcement precedes the bracket (the
                // pass itself carries no model_changed — state is
                // announced live, never reconstructed from history).
                "model_changed",
                "replay_begin",
                "user_message",
                "turn_started",
                "text_delta",
                "completion_call",
                "turn_committed",
                "replay_end",
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
                    origin: None,
                    event: crate::SessionEvent::TextDelta { text, .. },
                    ..
                }) if text == "first answer"
            )),
            "history arrives whole"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_fresh_boot_replays_nothing() {
        // Replay is default-on only for a RESUMED boot (owner ruling
        // 2026-09-25); a fresh boot (or an absorbed `--continue` miss)
        // has nothing to replay and emits no pass.
        let (code, frames) = bridge("no-replay", "", vec![script("hello")]).await;
        assert_eq!(code, 0);
        assert!(
            !frames.iter().any(|frame| matches!(
                frame,
                ServerFrame::Event(EventFrame {
                    origin: None,
                    event: crate::SessionEvent::ReplayBegin { .. },
                    ..
                })
            )),
            "a fresh boot replays nothing"
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
        let open: crate::OpenSessionSource = Arc::new(move |id: &str| {
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
keyless = true

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
            .model_factory(Arc::new(move |_, _, _| {
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
                node: std::sync::Arc::new(crate::Node::new("test")),
                boot_parent: None,
                boot_parent_call: None,
                store: SessionStore::new(&dir),
            },
            SessionHostData {
                create,
                open,
                extensions: Default::default(),
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
        let boot = boot_session_id(&out).await;
        tx_in.send(message_line(&boot, "hello")).unwrap();
        await_line(&out, "run_finished").await;

        // A second session, by command: its `session_opened` announce
        // (v10 — one shape for every path) carries its id and stamps
        // its own stream. The output buffer is append-only and the
        // BOOT's announce line is forever the first `session_opened`
        // line in it — scan for the first announce that is NOT the
        // boot's own (`await_line` alone cannot express that).
        tx_in.send(r#"{"type":"new_session"}"#.to_string()).unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let (created, created_line) = loop {
            let found = read_lines(&out)
                .into_iter()
                .filter(|line| line.contains("session_opened"))
                .find_map(|line| match serde_json::from_str::<ServerFrame>(&line) {
                    Ok(ServerFrame::Event(EventFrame {
                        origin: None,
                        stream: Some(stream),
                        event: crate::SessionEvent::SessionOpened { id, .. },
                        ..
                    })) if id != boot => Some((id, stream, line)),
                    _ => None,
                });
            match found {
                Some((id, stream, line)) => {
                    assert_eq!(
                        stream.as_str(),
                        id,
                        "the announce is stamped with the new session's own stream"
                    );
                    break (id, line);
                }
                None => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the new session's announce must arrive before timeout"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            }
        };
        assert_ne!(created, boot, "the new session is a second stream");
        assert!(created_line.contains(&format!(r#""id":"{created}""#)));

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
        await_line(&out, "replay_end").await;
        let replay_started = await_line(&out, "replay_begin").await;
        assert!(replay_started.contains(&format!(r#""stream":"{boot}""#)));

        drop(tx_in);
        assert_eq!(serve_task.await.unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_stream_end_resolves_the_edge_without_client_eof() {
        // The open-and-quiet bug: when the host behind the edge wound
        // down while the client's input stayed open, serve parked on
        // the reader forever — the connection hung silent with nothing
        // left behind it. The stream's end must resolve the edge
        // instead (the forwarder drains what landed first, then the
        // writer follows it out). The wind-down here rides the polite
        // door (close_commands) — the same token a worker-death
        // wind-down would fire.
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let session = test_session("stream-end", vec![script("pong")]);
        let mut handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&test_dir("stream-end")),
            test_data(unusable_create()),
        );
        handle.close_commands();
        let out = SharedOut::default();
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve(
                handle,
                ChannelIn {
                    lines: rx_in,
                    buf: Vec::new(),
                },
                out.clone(),
            ),
        )
        .await
        .expect("the stream's end resolves the edge without EOF");
        assert_eq!(code, 0);
        // The client never closed — the input sender is still held
        // here, so the resolution came from the stream's end, not an
        // EOF read.
        drop(tx_in);
        let _ = std::fs::remove_dir_all(test_dir("stream-end"));
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
        let handle = SessionHost::spawn(
            session,
            Vec::new(),
            test_wiring(&dir),
            test_data(unusable_create()),
        );
        let out = SharedOut::default();
        // The piped-burst shape (message + immediate EOF) needs the
        // session id, which is only knowable after the announce — so
        // drive it live: learn the id, send the message, and close
        // input immediately.
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));
        let session_id = boot_session_id(&out).await;
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
            test_wiring(&test_dir("checkout")),
            test_data(unusable_create()),
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

        let session_id = boot_session_id(&out).await;
        tx_in.send(message_line(&session_id, "hi")).unwrap();
        await_line(&out, "run_finished").await;

        // The checkout target is learnable only from the user_message
        // line — the honest client shape (entry ids come from events).
        let entry_id = read_lines(&out)
            .into_iter()
            .find_map(|line| match serde_json::from_str::<ServerFrame>(&line) {
                Ok(ServerFrame::Event(EventFrame {
                    origin: None,
                    event: crate::SessionEvent::UserMessage { text, entry_id },
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
        await_line(&out, "replay_end").await;

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
