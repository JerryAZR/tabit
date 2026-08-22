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
use tabit_session::{SessionCommandLink, SessionHandle, SessionInfo};
use tokio::sync::mpsc;

/// Serve the session over `reader`/`writer` until the client closes its
/// input. Returns the process exit code: 0 normally, 1 on a handshake
/// version mismatch (the connection is rejected and closed).
pub async fn serve<R, W>(mut handle: SessionHandle, reader: R, writer: W) -> i32
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let (writer_tx, writer_rx) = mpsc::unbounded_channel::<ServerFrame>();
    let link = handle.command_link();
    let info = handle.info().clone();

    // The reader blocks on lines; the writer is the single owner of
    // stdout, draining one ordered channel so the handshake ack can
    // never land behind an event (no event exists before the first
    // command, which is sent only after the ack).
    let reader_tx = writer_tx.clone();
    let reader_task =
        tokio::task::spawn_blocking(move || read_loop(reader, link, reader_tx, &info));
    let writer_task = tokio::spawn(write_loop(writer_rx, writer));

    // The live forwarder: actor events reach stdout as they happen,
    // for the whole session — not only at wind-down. (v1 bug: events
    // accumulated unread in the actor's channel until the client
    // closed stdin; the GUI sat at "queued" forever while the run was
    // already streaming.)
    let forwarder_task = tokio::spawn(forward_events(handle.take_events(), writer_tx.clone()));

    // A panicked reader thread is a broken edge: exit nonzero.
    let exit = reader_task.await.unwrap_or(1);
    handle.close_commands();
    // The forwarder ends when the worker drops the event sender at
    // wind-down (accepted messages still run); the writer ends when
    // every writer_tx clone is gone.
    drop(writer_tx);
    let _ = forwarder_task.await;
    let _ = writer_task.await;
    exit
}

/// Pump the actor's event stream into the writer channel until the
/// stream ends.
async fn forward_events(
    stream: Option<mpsc::UnboundedReceiver<EventFrame>>,
    out: mpsc::UnboundedSender<ServerFrame>,
) {
    let Some(mut stream) = stream else {
        return;
    };
    while let Some(frame) = stream.recv().await {
        let _ = out.send(ServerFrame::Event(frame));
    }
}

/// Parse client lines and act on them until EOF (or a rejected
/// handshake). Blocking, runs on its own thread.
fn read_loop<R: BufRead>(
    mut reader: R,
    link: SessionCommandLink,
    out: mpsc::UnboundedSender<ServerFrame>,
    info: &SessionInfo,
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
            Ok(ClientFrame::Initialize { protocol_version }) if !initialized => {
                if protocol_version == PROTOCOL_VERSION {
                    control(
                        &out,
                        ServerControlFrame::InitializeAck {
                            protocol_version: PROTOCOL_VERSION,
                            session_id: info.session_id.clone(),
                            session_path: info.session_path.clone(),
                            model: info.model.clone(),
                        },
                    );
                    initialized = true;
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
        let Ok(line) = serde_json::to_string(&frame) else {
            continue; // events are always serializable; skip defensively
        };
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
    use tabit_session::{EventFrame, ModelSelection, Session, SessionBuilder, SessionStore};

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

    fn test_session(tag: &str, turns: Vec<Vec<MockStreamEvent>>) -> Session {
        let dir = std::env::temp_dir()
            .join("tabit-json-tests")
            .join(format!("{tag}-{}", std::process::id()));
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
        SessionBuilder::new(
            SessionStore::new(&dir),
            config,
            auth,
            ModelSelection::new("p", "m"),
        )
        .expect("builder")
        .model_factory(move |_, _| {
            Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
                turns.clone(),
            )))
        })
        .create("C:/w")
        .expect("session")
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
    /// and the parsed server lines.
    async fn bridge(
        tag: &str,
        input: &str,
        turns: Vec<Vec<MockStreamEvent>>,
    ) -> (i32, Vec<ServerFrame>) {
        let session = test_session(tag, turns);
        let handle = SessionHandle::spawn(session);
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

    #[tokio::test]
    async fn events_stream_before_the_client_closes_input() {
        // Regression: the bridge forwarded actor events only at
        // wind-down, so a live client saw nothing until it closed
        // stdin (the GUI sat at "queued" while the run streamed).
        let (tx_in, rx_in) = std::sync::mpsc::channel::<String>();
        let session = test_session("live", vec![script("pong")]);
        let handle = SessionHandle::spawn(session);
        let out = SharedOut::default();
        let serve_task = tokio::spawn(serve(
            handle,
            ChannelIn {
                lines: rx_in,
                buf: Vec::new(),
            },
            out.clone(),
        ));

        tx_in.send(r#"{"protocol_version":1}"#.to_string()).unwrap();
        tx_in
            .send(r#"{"type":"message","text":"hi"}"#.to_string())
            .unwrap();

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
                    event: tabit_session::SessionEvent::UserMessage { text },
                    ..
                }) if kind == "user" => Some(text.as_str()),
                ServerFrame::Event(EventFrame {
                    event: tabit_session::SessionEvent::TextDelta { text },
                    ..
                }) if kind == "delta" => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn handshake_then_round_trip_over_memory_buffers() {
        let (code, frames) = bridge(
            "roundtrip",
            "{\"protocol_version\":1}\n{\"type\":\"message\",\"text\":\"hi\"}\n",
            vec![script("hello")],
        )
        .await;
        assert_eq!(code, 0);
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
        let (code, frames) = bridge(
            "preinit",
            "{\"type\":\"abort\"}\n{\"protocol_version\":1}\n{\"type\":\"message\",\"text\":\"real\"}\n",
            vec![script("ok")],
        )
        .await;
        assert_eq!(code, 0);
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
    async fn an_unparseable_line_is_reported_and_skipped() {
        // A blank line between frames is skipped, and a second
        // initialize is a protocol error, not a restart.
        let (code, frames) = bridge(
            "garbage",
            "this is not json\n\n{\"protocol_version\":1}\n{\"protocol_version\":1}\n",
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
        let handle = SessionHandle::spawn(session);
        let code = serve(handle, PanickingReader, SharedOut::default()).await;
        assert_eq!(code, 1);

        // Erroring reader: reads as EOF, exit 0.
        let session = test_session("error-reader", vec![script("x")]);
        let handle = SessionHandle::spawn(session);
        let code = serve(handle, ErroringReader, SharedOut::default()).await;
        assert_eq!(code, 0);

        // Failing writer: the writer stops, the rest winds down, exit 0.
        let session = test_session("fail-writer", vec![script("x")]);
        let handle = SessionHandle::spawn(session);
        let code = serve(
            handle,
            Cursor::new(b"{\"protocol_version\":1}\n".to_vec()),
            FailingWriter,
        )
        .await;
        assert_eq!(code, 0);
    }
}
