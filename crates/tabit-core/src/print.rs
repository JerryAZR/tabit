//! Print mode — the smallest frontend: one prompt in, the agent's
//! response printed once at the run's terminal, interaction cards
//! answered from stdin (Esc aborts the running turn). It rides the
//! same session host and extension world as JSON mode — the one path
//! law (the modes differ only at this I/O arm: no wire frames in,
//! rendered text out). stdout carries exactly the response text, one
//! copy at the terminal — the deltas are buffered, never streamed;
//! every other rendering (markers, cards, diagnostics) is stderr.

use std::io::Write as _;

use tabit_protocol::SessionCommand;
use tabit_session::SessionEvent;
use tabit_session::{ModelRegistry, Session, SessionHost, SessionHostWiring, SessionStore};

use crate::cli::Args;
use tabit_app::extensions;
use tabit_app::{ContinueMiss, assemble, host_data, host_node};

pub(crate) fn list_sessions(store: &SessionStore) -> Result<(), String> {
    let summaries = store.list().map_err(|e| e.to_string())?;
    if summaries.is_empty() {
        println!("no sessions in {}", store.dir().display());
        return Ok(());
    }
    for summary in summaries {
        println!(
            "{}  {:>4} entries  {:<10}  {}",
            summary.created_at,
            summary.entry_count,
            summary.id.get(..8).map(str::to_string).unwrap_or_default(),
            summary.path.display()
        );
    }
    Ok(())
}

fn print_event(event: &SessionEvent) {
    // The diagnostics renderer: everything here is stderr — stdout
    // is the answer channel, and the answer prints once, at the
    // terminal, from the run loop's buffer.
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    match event {
        SessionEvent::UserMessage { .. } => {}
        // The submit-time ack for messages that wait; print mode cannot
        // submit mid-run (Esc aborts), so this never fires in practice.
        SessionEvent::MessageQueued { .. } => {}
        SessionEvent::SkillsAvailable { .. } => {}
        // Backend-level catalogs never ride a run's print stream.
        SessionEvent::ExtensionsAvailable { .. } => {}
        SessionEvent::MessagesDiscarded { messages } => {
            let _ = writeln!(out, "[{} queued message(s) discarded]", messages.len());
        }
        // Cards render on stderr in the event loop; stdout stays the
        // answer channel. The settle close follows the card: print
        // mode's card loop ends with the answer, so there is nothing
        // left to close here.
        SessionEvent::InteractionRequest { .. } | SessionEvent::InteractionSettled { .. } => {}
        SessionEvent::RunAborted { .. } => {
            let _ = writeln!(
                out,
                "
[aborted]"
            );
        }
        // TextDelta never reaches this renderer: the run loop
        // buffers the response text and prints it once at the
        // terminal. Reasoning is diagnostics too (never the answer).
        SessionEvent::TextDelta { .. } => {}
        SessionEvent::ReasoningDelta { reasoning, .. } => {
            let _ = out.write_all(reasoning.as_bytes());
            let _ = out.flush();
        }
        SessionEvent::ToolCall {
            name, arguments, ..
        } => {
            let _ = writeln!(out, "\n→ {name} {}", arguments.as_deref().unwrap_or(""));
            let _ = out.flush();
        }
        SessionEvent::ToolResult { name, .. } => {
            let _ = writeln!(out, "← {name} done");
        }
        SessionEvent::TurnRetried { .. } => {
            let _ = writeln!(out, "[turn rejected by a hook; retrying]");
        }
        SessionEvent::CompletionCall { .. } => {}
        // Turn brackets are attribution machinery (the GUI's grouping);
        // the terminal view shows content as it streams.
        SessionEvent::TurnStarted { .. } | SessionEvent::TurnCommitted { .. } => {}
        // Informational (ENGINE.md behavior delta 9): the run continues;
        // the note is the user's cue that a steer can ask for more.
        SessionEvent::TurnTruncated { .. } => {
            let _ = writeln!(out, "[model output was truncated (output token limit)]");
        }
        SessionEvent::RunFinished { durable: false, .. } => {
            let _ = writeln!(out, "[output pending on disk — persist degraded]");
        }
        SessionEvent::RunFinished { .. } => {}
        // Not a printable stream event: run() turns it into the process
        // error (stderr, exit 1) once the stream has ended.
        SessionEvent::RunFailed { .. } => {}
        // Replay brackets DO arrive on resumed print boots (replay is
        // default-on since v19) and render nothing — the terminal
        // does not reconstruct history. Checkouts and model changes
        // never reach print mode (no checkout surface, no picker);
        // the arms exist for exhaustiveness.
        SessionEvent::ReplayBegin { .. }
        | SessionEvent::ReplayEnd
        | SessionEvent::CheckedOut { .. }
        | SessionEvent::ModelChanged { .. } => {}
        // The compaction bracket (v7): stdout stays the answer channel,
        // so the boundaries note on stderr and the summary stays quiet
        // in print mode.
        SessionEvent::CompactionBegin => {
            let _ = writeln!(std::io::stderr(), "[compacting the conversation…]");
        }
        SessionEvent::CompactionDelta { .. }
        | SessionEvent::CompactionStep { .. }
        | SessionEvent::CompactionRetried
        | SessionEvent::CompactionEnd { .. } => {}
        SessionEvent::CompactionFailed { message, .. } => {
            let _ = writeln!(std::io::stderr(), "warning: compaction failed: {message}");
        }
        // The host's session catalog and creations are frontend
        // concerns; print mode is a single-session consumer.
        SessionEvent::SessionsAvailable { .. } | SessionEvent::SessionOpened { .. } => {}
        // Non-terminal error conditions (startup degradations,
        // persistence): stderr is the human surface in print mode —
        // stdout stays the answer channel.
        SessionEvent::Error { message, .. } => {
            let _ = writeln!(std::io::stderr(), "warning: {message}");
        }
        SessionEvent::NativeItem { .. } => {}
    }
}

/// The human startup banner (stderr — stdout is the answer channel in
/// print mode and the protocol channel in JSON mode).
pub(crate) fn print_banner(session: &Session) {
    let stats = session.stats();
    if stats.total_usage.total_tokens > 0 {
        eprintln!(
            "resuming {} ({} prior turns of context)",
            session
                .id()
                .get(..8)
                .map(str::to_string)
                .unwrap_or_default(),
            session.context().len()
        );
    } else {
        let where_ = session
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "in memory".to_string());
        eprintln!("session {where_} started");
    }
}

/// What one print-mode session left behind, for the footer and exit code.
struct PrintOutcome {
    failed: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    session_path: String,
    stats: Option<tabit_session::SessionStats>,
}

/// Print mode: assemble (rewinding first when asked), banner, one
/// message through the session actor, events printed as they arrive,
/// then the closing footer. The extension world is the caller's —
/// booted on `runtime` before this runs (the serving runtime: the
/// supervisor's watchers must outlive the boot), `None` only when
/// the process booted no extension root.
pub(crate) fn print_mode(
    args: &Args,
    registry: &ModelRegistry,
    extensions: Option<&std::sync::Arc<extensions::Mounted>>,
    runtime: &tokio::runtime::Runtime,
) -> Result<i32, String> {
    if args.rewind.is_some() && args.session.is_none() && !args.continue_newest {
        return Err(
            "--rewind rewinds a session: pass --continue or --session <path> (see --help)"
                .to_string(),
        );
    }
    let options = args.options();
    let (mut session, startup_notes) = assemble(
        &options,
        registry,
        &SessionStore::project_default(),
        ContinueMiss::Fail,
        extensions.cloned(),
    )?;
    if let Some(turns) = args.rewind {
        let rewind = session.rewind(turns).map_err(|e| e.to_string())?;
        eprintln!(
            "[rewound: dropped {} user message(s) — the next prompt branches from before them]",
            rewind.dropped
        );
    }
    // A promptless rewind is complete: the marker alone carries it.
    let Some(prompt) = args.print_prompt.clone() else {
        return Ok(0);
    };

    print_banner(&session);

    // One stdin reader owns both duties (line-buffered stdin in print
    // mode: press Esc then Enter to abort; any other line answers the
    // open interaction card — its number for buttons, free text
    // otherwise).
    let armed: ArmedSlot = std::sync::Arc::default();

    // The message goes through the session host — the same path JSON
    // mode drives — and the stream is read to its end: the host returns
    // the session before closing, so closing stats cover this run.
    let outcome = runtime.block_on(async {
        let empty_mount;
        let mounted = match extensions {
            Some(mounted) => mounted,
            None => {
                empty_mount = std::sync::Arc::new(extensions::Mounted::none());
                &empty_mount
            }
        };
        let store = SessionStore::project_default();
        let wiring = SessionHostWiring {
            node: host_node(),
            store: store.clone(),
            boot_parent: args.parent.clone(),
            boot_parent_call: args.parent_call.clone(),
        };
        let data = host_data(&options, registry, &store, mounted);
            let mut handle = SessionHost::spawn(session, startup_notes, wiring, data);
            let boot = handle.info().session_id.clone();
            {
                let link = handle.command_link();
                let armed = armed.clone();
                let boot = boot.clone();
                std::thread::spawn(move || {
                    use std::io::BufRead as _;
                    for line in std::io::stdin().lock().lines().by_ref().flatten() {
                        if line.starts_with('\x1b') {
                            link.send(SessionCommand::Abort {
                                session: boot.clone(),
                            });
                            return;
                        }
                        // Answers apply to the oldest open card (FIFO —
                        // FRONTEND.md §8 allows several open at once, and
                        // concurrent permission gates make that ordinary).
                        let card = { lock_armed(&armed).pop_front() };
                        if let Some((id, options)) = card {
                            link.send(parse_answer(&boot, &id, &options, &line));
                            let waiting = lock_armed(&armed).len();
                            if waiting > 0 {
                                eprintln!("--- {waiting} more open question(s), keep answering");
                            }
                        }
                    }
                });
            }
            let mut outcome = PrintOutcome {
                failed: None,
                input_tokens: 0,
                output_tokens: 0,
                session_path: handle.info().session_path.clone(),
                stats: None,
            };
            // The response text, one copy: the deltas accumulate here
            // and stdout sees it exactly once, at the run's terminal
            // (the owner's print-mode law — stdout is the answer).
            let mut answer = String::new();
            handle.message(&boot, prompt);
            handle.close_commands();
            while let Some(frame) = handle.next_event().await {
                match &frame.event {
                    SessionEvent::TextDelta { text, .. } => {
                        answer.push_str(text);
                    }
                    SessionEvent::CompletionCall { usage, .. } => {
                        outcome.input_tokens += usage.input_tokens;
                        outcome.output_tokens += usage.output_tokens;
                    }
                    SessionEvent::RunFailed { message, .. } => {
                        outcome.failed = Some(message.clone());
                        // A terminal closes every card (FRONTEND.md §8).
                        lock_armed(&armed).clear();
                    }
                    SessionEvent::InteractionRequest {
                        id,
                        ui_type,
                        payload,
                        ..
                    } => {
                        // A template consumer like any frontend: render
                        // the natives, report the rest (never answer a
                        // widget this surface cannot construct). Print
                        // mode's select_any rendering is single-select
                        // (number picks one); multi-select needs a
                        // compositing frontend.
                        use tabit_protocol::templates;
                        match ui_type.as_str() {
                            templates::ui::SELECT_ONE | templates::ui::SELECT_ANY => {
                                let (title, body, options) = if ui_type == templates::ui::SELECT_ONE
                                {
                                    let Ok(card) = serde_json::from_value::<
                                        templates::SelectOneCard,
                                    >(payload.clone()) else {
                                        eprintln!(
                                            "(a select card arrived in a shape this surface cannot read)"
                                        );
                                        continue;
                                    };
                                    (card.title, card.body, card.options)
                                } else {
                                    let Ok(card) = serde_json::from_value::<
                                        templates::SelectAnyCard,
                                    >(payload.clone()) else {
                                        eprintln!(
                                            "(a select card arrived in a shape this surface cannot read)"
                                        );
                                        continue;
                                    };
                                    (card.title, card.body, card.options)
                                };
                                eprintln!(
                                    "
--- {title}
{body}"
                                );
                                if options.is_empty() {
                                    eprintln!("(type your answer, then Enter)");
                                } else {
                                    let legend = options
                                        .iter()
                                        .enumerate()
                                        .map(|(n, o)| format!("{}) {}", n + 1, o.label))
                                        .collect::<Vec<_>>()
                                        .join("  ");
                                    eprintln!("{legend}   — number, then Enter");
                                }
                                let mut queue = lock_armed(&armed);
                                queue.push_back((
                                    id.clone(),
                                    options.into_iter().map(|o| o.label).collect(),
                                ));
                                if queue.len() > 1 {
                                    eprintln!(
                                        "({} open questions — answers apply in order)",
                                        queue.len()
                                    );
                                }
                            }
                            other => eprintln!(
                                "(unsupported interaction widget `{other}` — not answered)"
                            ),
                        }
                    }
                    SessionEvent::RunFinished { .. } | SessionEvent::RunAborted { .. } => {
                        // A terminal closes every card (FRONTEND.md §8).
                        lock_armed(&armed).clear();
                        // The answer channel's one write: the buffered
                        // response (an abort prints its partial text —
                        // that is what was said; a failure's report is
                        // the process error, not stdout).
                        if !answer.is_empty() {
                            let stdout = std::io::stdout();
                            let mut out = stdout.lock();
                            let _ = writeln!(out, "{answer}");
                            let _ = out.flush();
                        }
                    }
                    _ => {}
                }
                print_event(&frame.event);
            }
            outcome.stats = handle.closing_stats();
            outcome
        });

    eprintln!(
        "--- session {} | tokens {} in / {} out{}",
        outcome.session_path,
        outcome.input_tokens,
        outcome.output_tokens,
        outcome
            .stats
            .map(|s| format!(" (session total {:.4} USD)", s.total_cost))
            .unwrap_or_default()
    );
    match outcome.failed {
        Some(message) => Err(format!("run failed: {message}")),
        None => Ok(0),
    }
}

/// One open interaction card: its request id, widget type, and button
/// labels, waiting for one stdin line. Several may be open at once
/// (concurrent gates); answers apply FIFO.
/// One open card awaiting its answer: the request id and
/// the option labels (numbered answers resolve against them).
type ArmedCard = (String, Vec<String>);
type ArmedSlot = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ArmedCard>>>;

/// Lock the armed-card queue (poisoning recovers — the queue is only a
/// hint for the stdin reader).
fn lock_armed(
    armed: &ArmedSlot,
) -> std::sync::MutexGuard<'_, std::collections::VecDeque<ArmedCard>> {
    armed.lock().unwrap_or_else(|error| error.into_inner())
}

#[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization (AGENTS.md doctrine)
fn parse_answer(session: &str, id: &str, options: &[String], line: &str) -> SessionCommand {
    use tabit_protocol::templates;
    let line = line.trim();
    // Numbered options parse as `2` or `2 reason`; a free-text card (no
    // options) takes the whole line; anything else answers with nothing
    // (the backend's fail-closed default, so a card can never hang).
    // Both select templates share the one SelectAnswer shape.
    let answer = if line.is_empty() || options.is_empty() {
        templates::SelectAnswer {
            selected: Vec::new(),
            text: (!line.is_empty() && options.is_empty()).then(|| line.to_string()),
        }
    } else {
        let (number, reason) = match line.split_once(char::is_whitespace) {
            Some((number, reason)) => (number, reason.trim()),
            None => (line, ""),
        };
        let option = number
            .parse::<usize>()
            .ok()
            .and_then(|n| options.get(n.checked_sub(1)?))
            .map(String::as_str);
        templates::SelectAnswer {
            selected: option.map(|o| vec![o.to_string()]).unwrap_or_default(),
            text: (!reason.is_empty()).then(|| reason.to_string()),
        }
    };
    let payload = serde_json::to_value(answer).expect("template payloads always serialize");
    SessionCommand::InteractionResponse {
        session: Some(session.to_string()),
        id: id.to_string(),
        payload,
    }
}

/// Turn one stdin line into the card's answer. Numbered buttons parse as
/// `2` or `2 reason text`; a free-text card takes the whole line. An
/// empty or unrecognizable line answers with nothing — the backend's
/// fail-closed default (deny / dismissed), so a card can never hang.
#[cfg(test)]
mod interaction_answer_tests {
    use super::*;

    fn options() -> Vec<String> {
        vec![
            "Allow".to_string(),
            "Always allow".to_string(),
            "Deny".to_string(),
        ]
    }

    fn answer(
        session: &str,
        id: &str,
        options: &[String],
        line: &str,
    ) -> (Vec<String>, Option<String>) {
        match parse_answer(session, id, options, line) {
            SessionCommand::InteractionResponse { payload, .. } => {
                let parsed: tabit_protocol::templates::SelectAnswer =
                    serde_json::from_value(payload).expect("the template payload parses");
                (parsed.selected, parsed.text)
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn numbered_buttons_select_by_index_with_optional_reason() {
        assert_eq!(
            answer("s1", "i1", &options(), "1"),
            (vec!["Allow".to_string()], None)
        );
        assert_eq!(
            answer("s1", "i2", &options(), "3 never delete build dirs"),
            (
                vec!["Deny".to_string()],
                Some("never delete build dirs".to_string())
            )
        );
    }

    #[test]
    fn free_text_cards_take_the_whole_line() {
        assert_eq!(
            answer("s1", "i3", &[], "use python"),
            (Vec::new(), Some("use python".to_string()))
        );
    }

    #[test]
    fn empty_or_unknown_answers_fail_closed_with_nothing() {
        assert_eq!(answer("s1", "i4", &options(), ""), (Vec::new(), None));
        assert_eq!(answer("s1", "i5", &options(), "   "), (Vec::new(), None));
        // Out-of-range numbers carry no option: the backend's default
        // (deny for permission) applies rather than a wrong button.
        assert_eq!(answer("s1", "i6", &options(), "9"), (Vec::new(), None));
    }
}
