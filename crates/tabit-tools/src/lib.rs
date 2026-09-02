#![cfg_attr(
    test,
    allow(
        clippy::err_expect,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::panic_in_result_fn,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]
//! Tabit's coding tools: file reading and shell execution, implemented
//! as contextual `#[rig_tool]`s (every tool reads the per-run
//! [`ToolContext`]) and erasable into [`DynamicTool`]s for session
//! registration.
//!
//! All paths are taken verbatim from the model; relative paths resolve
//! against the session's working directory (the `SessionCwd`
//! capability — absent it, the process working directory, the same
//! bytes the OS would resolve). Errors are user-facing (external
//! errors): clear, graceful, and never a panic.
//!
//! Native only: these tools touch the filesystem and spawn processes.
//!
//! # Cancellation contract (tool authors read this)
//!
//! Cancellation is cooperative and split by ownership — the engine
//! owns *when* to stop, the tool owns *how* to stop what it started.
//! Three layers:
//!
//! - **The token is the ask**: the runtime cancels a per-invocation
//!   [`CancellationToken`](tokio_util::sync::CancellationToken) on
//!   abort (user stop, session shutdown). Tools receive it through
//!   [`ToolContext`]; plain `#[rig_tool]` functions that never spawn
//!   OS resources can ignore it. On native, bodies poll on an
//!   isolated sidecar runtime (ENGINE.md's execution-substrate
//!   ruling), so abort does not drop the body mid-poll — the token
//!   is the mechanism, and a well-behaved body observes it. The
//!   `bash` tool is the reference implementation: it spawns its
//!   child through `process-wrap` (`JobObject` on Windows, a
//!   process-group leader on Unix) so an explicit `kill()` — and
//!   the drop backstop when the body ends — take down the whole
//!   process tree, and it reads both output pipes up front so a
//!   dead child's pipe never deadlocks the reader.
//! - **Bounded bodies are the expectation**: every chain is bounded
//!   by its own timeout or the user. A body that ignores the token
//!   cannot be force-killed (Rust has no safe thread-kill) — it
//!   leaks a sidecar task until it returns, never stalling the
//!   harness; blocking the thread is safe but wasteful, so prefer
//!   async in bodies. Bodies get a full tokio context on native.
//! - **Process death is the backstop**: on session/process exit all
//!   threads and children die with it.
//! - **Force, no grace**: kills go straight to force (no SIGTERM
//!   grace period). Tool calls are user-cancellable and the model
//!   is told the call was interrupted, so there is no cleanup
//!   contract with the child. Where a grace-then-kill is ever
//!   wanted it lives at the resource-acquisition boundary (the
//!   spawner), never in the engine.
//! - **Report shape**: a cancelled call returns a clear
//!   "interrupted" error/result (the session layer synthesizes the
//!   model-visible record for calls that never answered); it never
//!   returns output that looks like a completed run.

use rig_agent::tool::{DynamicTool, ToolContext};
use rig_core::tool::{IntoToolOutput, ToolExecutionError};
use rig_derive::rig_tool;
use std::time::{Duration, Instant};

mod diff;
mod file_io;
mod shell;
mod truncate;

/// Default seconds a `bash` command may run.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Maximum seconds a `bash` command may run.
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// Resolve a model-given path for this run: absolute paths pass
/// through; relative paths join the session's working directory (the
/// [`SessionCwd`](rig_agent::tool::SessionCwd) capability, mounted by
/// the run's opener), falling back to the process cwd for standalone
/// tool use — the same bytes the OS would resolve today.
fn resolve(context: &ToolContext, path: &str) -> std::path::PathBuf {
    let given = std::path::Path::new(path);
    if given.is_absolute() {
        return given.to_path_buf();
    }
    match context
        .get::<rig_agent::tool::SessionCwd>()
        .map(|cwd| cwd.0.clone())
    {
        Some(cwd) => cwd.join(given),
        // No session scope: a bare relative path, resolved against the
        // process cwd by the OS itself — today's semantics.
        None => std::path::PathBuf::from(path),
    }
}

/// The session's working directory for spawned commands, when the run
/// is session-scoped.
fn session_cwd(context: &ToolContext) -> Option<std::path::PathBuf> {
    context
        .get::<rig_agent::tool::SessionCwd>()
        .map(|cwd| cwd.0.clone())
}

/// Read a UTF-8 text file page by page, or an image whole. Text output
/// is capped at 50 KiB of whole lines ([`truncate`]) and every
/// truncation notice carries the offset that continues the read: large
/// files are paged, never spilled (the file is already on disk).
/// Images (PNG/JPEG/GIF/WebP, by magic bytes) bypass the text cap
/// entirely and ride as image content parts, capped at
/// [`IMAGE_MAX_BYTES`] — no resize in v1, so oversized images are
/// rejected with guidance (downscaling is a dependency decision,
/// deferred). Directories list their entries inline. Binary and
/// UTF-16/32 text files are rejected loudly. Relative paths resolve
/// against the session's working directory.
#[rig_tool(description = "Read a file (absolute or relative path; relative \
                   paths resolve against the session's working directory). \
                   Text files: UTF-8, output capped at 50 KiB in whole lines, \
                   the truncation notice carries the offset to continue \
                   from; optional offset (1-indexed line number) and limit \
                   (line count) page large files. Images (PNG, JPEG, GIF, \
                   WebP): sent to the model as an image, up to 3 MiB. \
                   Reading a directory lists its entries.")]
pub async fn read(
    #[rig(context)] context: &mut ToolContext,
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<rig_core::OneOrMany<rig_core::message::ToolResultContent>, ToolExecutionError> {
    let resolved = resolve(context, &path);
    let meta = std::fs::metadata(&resolved)
        .map_err(|e| ToolExecutionError::other(format!("cannot read `{path}`: {e}")))?;
    if meta.is_dir() {
        return report_text(list_directory(&resolved, &path)?);
    }
    let bytes = std::fs::read(&resolved)
        .map_err(|e| ToolExecutionError::other(format!("cannot read `{path}`: {e}")))?;
    if let Some(media_type) = image_media_type(&bytes) {
        return image_read(
            &path,
            &bytes,
            media_type,
            offset.is_some() || limit.is_some(),
        );
    }
    report_text(read_text(&path, &bytes, offset, limit)?)
}

/// The largest image read returns, raw bytes. The provider ceiling is
/// ~5 MiB of base64 (~3.75 MiB raw, Anthropic's per-image limit); 3 MiB
/// keeps headroom. v1 has no resize — an image over the cap is rejected
/// with guidance instead of silently downgraded.
pub(crate) const IMAGE_MAX_BYTES: usize = 3 * 1024 * 1024;

/// The image formats every provider carries (magic-byte detected);
/// matches pi's native set. HEIC/HEIF/SVG stay text-path files.
fn image_media_type(bytes: &[u8]) -> Option<rig_core::message::ImageMediaType> {
    use rig_core::message::ImageMediaType;
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some(ImageMediaType::PNG),
        [0xff, 0xd8, 0xff, ..] => Some(ImageMediaType::JPEG),
        [b'G', b'I', b'F', b'8', b'7', b'a', ..] | [b'G', b'I', b'F', b'8', b'9', b'a', ..] => {
            Some(ImageMediaType::GIF)
        }
        // RIFF container: the bytes at 8..12 name the codec.
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some(ImageMediaType::WEBP),
        _ => None,
    }
}

/// One image, whole: a text part naming the read plus the image part
/// itself (base64 — the session log and the provider wire both carry
/// base64; raw bytes would serialize as a JSON number array).
fn image_read(
    path: &str,
    bytes: &[u8],
    media_type: rig_core::message::ImageMediaType,
    paged_args: bool,
) -> Result<rig_core::OneOrMany<rig_core::message::ToolResultContent>, ToolExecutionError> {
    if paged_args {
        return Err(ToolExecutionError::other(
            "images are read whole — offset/limit apply to text files only",
        ));
    }
    if bytes.len() > IMAGE_MAX_BYTES {
        return Err(ToolExecutionError::other(format!(
            "`{path}` is {} bytes; images are capped at {} bytes ({} KiB). \
             Downscale or crop it first, or extract a region with the shell tool",
            bytes.len(),
            IMAGE_MAX_BYTES,
            IMAGE_MAX_BYTES / 1024
        )));
    }
    use base64::Engine as _;
    use rig_core::message::ToolResultContent;
    let mime = rig_core::completion::message::MimeType::to_mime_type(&media_type).to_string();
    let report = format!("Read image `{path}` ({mime}, {} bytes)", bytes.len());
    let parts = vec![
        ToolResultContent::Text(report.into()),
        ToolResultContent::Image(rig_core::message::Image {
            data: rig_core::message::DocumentSourceKind::Base64(
                base64::engine::general_purpose::STANDARD.encode(bytes),
            ),
            media_type: Some(media_type),
            detail: None,
            additional_params: None,
        }),
    ];
    // Two parts by construction.
    #[allow(clippy::expect_used)]
    Ok(rig_core::OneOrMany::many(parts).expect("the parts vector is non-empty"))
}

/// Wrap one text report as the tool's content parts (every text-only
/// result is a single part).
fn report_text(
    report: String,
) -> Result<rig_core::OneOrMany<rig_core::message::ToolResultContent>, ToolExecutionError> {
    Ok(rig_core::OneOrMany::one(
        rig_core::message::ToolResultContent::Text(report.into()),
    ))
}

/// The text path, unchanged from the days `read` returned a bare
/// string: encoding checks, paging, truncation notices.
fn read_text(
    path: &str,
    bytes: &[u8],
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, ToolExecutionError> {
    if let Some(encoding) = non_utf8_bom(bytes) {
        return Err(ToolExecutionError::other(format!(
            "`{path}` is {encoding}-encoded; this tool reads UTF-8 only — \
             re-save the file as UTF-8 or inspect it with the shell tool"
        )));
    }
    let mut text = match String::from_utf8(bytes.to_vec()) {
        Ok(text) => text,
        Err(error) => {
            return Err(ToolExecutionError::other(format!(
                "`{path}` is not valid UTF-8 ({} bytes); binary files are not \
                 supported by this tool ({error})",
                error.utf8_error().valid_up_to()
            )));
        }
    };
    // Strip a UTF-8 BOM: what read shows must be exactly what edit will
    // match later — one file-reading convention for the text tools.
    if text.starts_with('\u{feff}') {
        text.replace_range(..'\u{feff}'.len_utf8(), "");
    }
    if text.is_empty() {
        return Ok("(file is empty)".to_string());
    }

    let lines = truncate::split_lines(&text);
    let total_lines = lines.len();
    // 1-indexed input, 0-indexed slicing; offset 0 reads as 1.
    let start = offset.unwrap_or(1).saturating_sub(1);
    if start >= total_lines {
        return Err(ToolExecutionError::other(format!(
            "offset {} is beyond the end of `{path}` ({total_lines} lines)",
            start + 1
        )));
    }
    let end = limit
        .map(|l| start.saturating_add(l).min(total_lines))
        .unwrap_or(total_lines);
    let selected = lines
        .get(start..end)
        .ok_or_else(|| ToolExecutionError::other(format!("empty line range {start}..{end}")))?
        .join("\n");

    let trunc = truncate::truncate_head(&selected);
    if trunc.first_line_exceeds_limit {
        let line_no = start + 1;
        return Ok(format!(
            "[Line {line_no} of `{path}` alone exceeds the 50 KiB output \
             limit. Use the shell tool to extract a slice of it (bash: \
             sed -n '{line_no}p' \"{path}\" | head -c 51200).]"
        ));
    }
    let mut out = trunc.content;
    if trunc.truncated {
        let last_shown = start + trunc.output_lines;
        out.push_str(&format!(
            "\n\n[Showing lines {}-{last_shown} of {total_lines}. \
             Use offset={} to continue.]",
            start + 1,
            last_shown + 1
        ));
    } else if end < total_lines {
        out.push_str(&format!(
            "\n\n[{} more lines in `{path}`. Use offset={} to continue.]",
            total_lines - end,
            end + 1
        ));
    }
    Ok(out)
}

/// A UTF-16/32 byte-order mark, if present — Windows tooling (PowerShell
/// redirects) writes these; naming the encoding beats a bare
/// invalid-UTF-8 report.
fn non_utf8_bom(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xff, 0xfe, 0x00, 0x00, ..] => Some("UTF-32 LE"),
        [0x00, 0x00, 0xfe, 0xff, ..] => Some("UTF-32 BE"),
        [0xff, 0xfe, ..] => Some("UTF-16 LE"),
        [0xfe, 0xff, ..] => Some("UTF-16 BE"),
        _ => None,
    }
}

/// Inline directory listing: a header naming the directory, then sorted
/// entry names with `/` on directories — lean on purpose (no size/mtime
/// columns; bash `ls -la` covers metadata). Capped through the shared
/// truncation. I/O runs on the resolved path; the header shows the
/// model-given one.
fn list_directory(resolved: &std::path::Path, display: &str) -> Result<String, ToolExecutionError> {
    let entries = std::fs::read_dir(resolved)
        .map_err(|e| ToolExecutionError::other(format!("cannot list `{display}`: {e}")))?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| ToolExecutionError::other(format!("listing `{display}`: {e}")))?;
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    let mut listing = format!("{display} is a directory:\n");
    if names.is_empty() {
        listing.push_str("(empty directory)");
    } else {
        listing.push_str(&names.join("\n"));
    }
    let trunc = truncate::truncate_head(&listing);
    let mut out = trunc.content;
    if trunc.truncated {
        // The header rides line 1 of the listing; entries follow.
        let shown = trunc.output_lines.saturating_sub(1);
        out.push_str(&format!(
            "\n\n[{shown} of {} entries shown; use the shell tool to page \
             the full listing]",
            names.len()
        ));
    }
    Ok(out)
}

/// Write bytes to a path: create new files freely; overwrite only when
/// the model said so (`overwrite: true`). The intent is expressed, never
/// inferred (owner ruling). Parent directories are created as needed and
/// named in the result. Mutations serialize through [`file_io`]'s
/// per-path lock; overwrites are atomic (temp-file + rename) so readers
/// never see a torn file. For targeted changes to existing files, use
/// edit. Relative paths resolve against the session's working
/// directory.
#[rig_tool(
    description = "Write a file (absolute or relative path; relative paths \
                   resolve against the session's working directory). Creates \
                   new files; overwrites an existing file only with \
                   overwrite: true — say so explicitly when replacing \
                   content. Parent directories are created as needed. To \
                   modify an existing file, prefer edit."
)]
pub async fn write(
    #[rig(context)] context: &mut ToolContext,
    path: String,
    content: String,
    overwrite: Option<bool>,
) -> Result<String, ToolExecutionError> {
    let resolved = resolve(context, &path);
    let display = path;
    let _guard = file_io::lock(&resolved).await;
    if resolved.is_dir() {
        return Err(ToolExecutionError::other(format!(
            "`{display}` is a directory — write needs a file path"
        )));
    }
    if resolved.exists() && overwrite != Some(true) {
        let size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
        return Err(ToolExecutionError::other(format!(
            "`{display}` already exists ({size} bytes). Pass overwrite: true \
             to replace it, or use edit for a targeted change."
        )));
    }
    let outcome = file_io::store(&resolved, content.as_bytes()).await?;
    let bytes = content.len();
    let parents = if outcome.parents_created > 0 {
        format!(", created {} parent dirs", outcome.parents_created)
    } else {
        String::new()
    };
    match outcome.previous_len {
        Some(was) => Ok(format!(
            "Overwrote {display} ({bytes} bytes, was {was} bytes{parents})"
        )),
        None => Ok(format!("Created {display} ({bytes} bytes{parents})")),
    }
}

/// Edit a file by exact text replacement: every edit is matched against
/// the file's current bytes (in LF space — the one sanctioned
/// normalization; the file's line endings and BOM are preserved on
/// write). Edits apply **independently** — those that match are applied,
/// those that don't are reported by index with the reason, so the model
/// fixes and resends only the failed ones. Overlapping edits apply only
/// when their shared region agrees; a conflicting pair is rejected
/// together. No fuzzy matching beyond line endings: a miss means the
/// model's view of the file is stale — read it fresh. Relative paths
/// resolve against the session's working directory.
#[rig_tool(
    description = "Edit a file with exact text replacements. Each edit is \
                   independent: those whose old_text matches a unique spot are \
                   applied, the rest are reported by index — fix and resend only \
                   the failed ones. old_text must match the file exactly \
                   (whitespace included; LF/CRLF differences are normalized). \
                   Overlapping edits apply only if their shared content agrees. \
                   Relative paths resolve against the session's working \
                   directory."
)]
pub async fn edit(
    #[rig(context)] context: &mut ToolContext,
    path: String,
    edits: Vec<EditReplacement>,
) -> Result<rig_core::OneOrMany<rig_core::message::ToolResultContent>, ToolExecutionError> {
    let resolved = resolve(context, &path);
    let _guard = file_io::lock(&resolved).await;
    let outcome = edit_core(&resolved.to_string_lossy(), &edits)?;
    report_with_details(outcome.report, outcome.details)
}

/// One tool result's content parts: the model-facing report text first,
/// then the structured details JSON when the tool produced any — the
/// session projects text → `content`, JSON → `tool_result.details`.
/// Every multi-part tool result (edit, the shell tools) is built here.
fn report_with_details(
    report: String,
    details: Option<serde_json::Value>,
) -> Result<rig_core::OneOrMany<rig_core::message::ToolResultContent>, ToolExecutionError> {
    let mut parts = vec![rig_core::message::ToolResultContent::Text(report.into())];
    if let Some(details) = details {
        parts.push(rig_core::message::ToolResultContent::Json { value: details });
    }
    // Invariant: one part minimum by construction above (the report).
    #[allow(clippy::expect_used)]
    Ok(rig_core::OneOrMany::many(parts).expect("the report part is always present"))
}

/// One targeted replacement: `old_text` must occur exactly once in the
/// file (LF-normalized); `new_text` replaces it.
#[derive(serde::Deserialize, rig_core::schemars::JsonSchema)]
#[serde(crate = "rig_core::serde")]
#[schemars(crate = "rig_core::schemars")]
pub struct EditReplacement {
    /// Exact text to replace (must occur exactly once in the file).
    pub old_text: String,
    /// Replacement text.
    pub new_text: String,
}

/// What one edit call produced: the model-facing report (the faithful
/// copy) and, when anything applied, the presentation cargo for
/// `tool_result.details` (the same facts, structured).
#[derive(Debug)]
struct EditOutcome {
    report: String,
    details: Option<serde_json::Value>,
}

/// The matching + application core, decoupled from the tool wrapper so
/// tests drive it directly. Contract (asserted by the test suite):
///
/// - match in LF-normalized space; restore the file's dominant line
///   ending and BOM on store;
/// - per-edit independence: matched edits apply, failures are reported
///   by index (empty old_text / not found / N occurrences / conflicting
///   overlap) — the applied set never shifts another edit's offsets
///   (application runs in reverse-offset order);
/// - overlapping edits apply only when their shared region agrees;
///   a conflicting pair rejects both;
/// - a call where nothing applies is an error (nothing was edited);
/// - the store rides [`file_io`]: per-path lock across read→match→store,
///   atomic persist.
fn edit_core(path: &str, edits: &[EditReplacement]) -> Result<EditOutcome, ToolExecutionError> {
    if edits.is_empty() {
        return Err(ToolExecutionError::other(
            "edits must contain at least one replacement".to_string(),
        ));
    }
    let display = path.to_string();
    let path = std::path::Path::new(path);
    let meta = std::fs::metadata(path)
        .map_err(|e| ToolExecutionError::other(format!("cannot read `{display}`: {e}")))?;
    if meta.is_dir() {
        return Err(ToolExecutionError::other(format!(
            "`{display}` is a directory — edit needs a file path"
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| ToolExecutionError::other(format!("cannot read `{display}`: {e}")))?;
    if bytes.is_empty() {
        return Err(ToolExecutionError::other(format!(
            "`{display}` is empty — nothing to match against; use write"
        )));
    }
    let raw = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            return Err(ToolExecutionError::other(format!(
                "`{display}` is not valid UTF-8 ({} bytes); binary files are \
                 not supported by this tool ({error})",
                error.utf8_error().valid_up_to()
            )));
        }
    };
    // The BOM is invisible to read, so it is invisible to matching; it is
    // re-attached on store.
    let (bom, content) = match raw.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", raw.as_str()),
    };
    let lf = content.replace("\r\n", "\n");
    let is_crlf = lf.len() != content.len();

    // --- match each edit independently in LF space ---
    let mut matched: Vec<Match> = Vec::new();
    let mut failures: Vec<(usize, String)> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let old = edit.old_text.replace("\r\n", "\n");
        if old.is_empty() {
            failures.push((i, "old_text is empty".to_string()));
            continue;
        }
        let occurrences: Vec<usize> = lf.match_indices(&old).map(|(at, _)| at).collect();
        match occurrences.as_slice() {
            [] => failures.push((
                i,
                "not found — the file may have changed; read it fresh".to_string(),
            )),
            [start] => {
                if edit.new_text == edit.old_text {
                    failures.push((i, "no change — old_text equals new_text".to_string()));
                } else {
                    matched.push(Match {
                        index: i,
                        start: *start,
                        end: start + old.len(),
                        new_text: edit.new_text.replace("\r\n", "\n"),
                    });
                }
            }
            many => failures.push((
                i,
                format!(
                    "{} occurrences — provide more context to make it unique",
                    many.len()
                ),
            )),
        }
    }

    // --- overlapping pairs: identical replacements are compatible
    //     (the shared region agrees — applying both yields the same
    //     result as one); anything else conflicts and rejects both.
    //     Equality is the check, not application-order simulation: two
    //     different replacements whose simulations agree can still
    //     scramble each other's span.
    matched.sort_by_key(|m| m.start);
    let mut accepted: Vec<Match> = Vec::new();
    for m in matched.into_iter() {
        if let Some(prev) = accepted.last_mut().filter(|prev| m.start < prev.end) {
            let compatible =
                prev.start == m.start && prev.end == m.end && prev.new_text == m.new_text;
            if compatible {
                accepted.push(m);
            } else {
                for (loser, other) in [(prev.index, m.index), (m.index, prev.index)] {
                    failures.push((
                        loser,
                        format!(
                            "conflicts with edit[{other}] on the overlapping \
                             region — merge them into one edit"
                        ),
                    ));
                }
                accepted.pop();
            }
            continue;
        }
        accepted.push(m);
    }

    if accepted.is_empty() {
        let mut report = format!("No edits applied to {display}.");
        for (i, reason) in &failures {
            report.push_str(&format!(" edit[{i}]: {reason}."));
        }
        return Err(ToolExecutionError::other(report));
    }

    // --- apply in reverse-offset order so earlier offsets stay valid ---
    let mut new_content = lf.clone();
    let mut accepted_rev: Vec<&Match> = accepted.iter().collect();
    accepted_rev.sort_by_key(|m| std::cmp::Reverse(m.start));
    for m in accepted_rev {
        new_content.replace_range(m.start..m.end, &m.new_text);
    }

    let after = new_content.clone();
    let stored = if is_crlf {
        new_content.replace('\n', "\r\n")
    } else {
        new_content
    };
    let stored = format!("{bom}{stored}");
    file_io::store_sync(path, stored.as_bytes())?;

    // --- report ---
    let (added, removed, first_line) = line_stats(&lf, &accepted);
    let mut out = format!(
        "Edited {display} ({} of {} block{} applied; +{added}/-{removed} lines, first change at line {first_line})",
        accepted.len(),
        edits.len(),
        if accepted.len() == 1 { "" } else { "s" },
    );
    for (i, reason) in &failures {
        out.push_str(&format!(" edit[{i}]: {reason}."));
    }

    // --- presentation cargo: the diff of the LF-space before/after plus
    //     every edit's outcome (the same facts the report carries,
    //     structured — one production site for the reason strings) ---
    let outcomes: Vec<diff::Outcome> = (0..edits.len())
        .map(|i| {
            if accepted.iter().any(|m| m.index == i) {
                diff::Outcome::Applied
            } else {
                let reason = failures
                    .iter()
                    .find(|(fi, _)| *fi == i)
                    .map(|(_, r)| r.clone())
                    .unwrap_or_else(|| "rejected".to_string());
                diff::Outcome::Rejected(reason)
            }
        })
        .collect();
    let details = diff::edit_details(&lf, &after, outcomes);

    Ok(EditOutcome {
        report: out,
        details: Some(details),
    })
}

/// A uniquely-matched edit: where it landed in the LF-normalized file
/// and what replaces the matched span.
struct Match {
    /// The edit's index in the call (for failure reports).
    index: usize,
    start: usize,
    end: usize,
    new_text: String,
}

/// (added, removed, first changed 1-indexed line) for the applied edits.
fn line_stats(lf: &str, accepted: &[Match]) -> (usize, usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut first_byte = usize::MAX;
    for m in accepted {
        removed += lf[m.start..m.end].matches('\n').count();
        added += m.new_text.matches('\n').count();
        first_byte = first_byte.min(m.start);
    }
    let first_line = lf[..first_byte].matches('\n').count() + 1;
    (added, removed, first_line)
}

/// Ask the user a question and return their answer — the whole body is
/// one interaction roundtrip over the session's
/// [`UserInteraction`](rig_agent::tool::interaction::UserInteraction)
/// capability (ENGINE.md's tool phase: a tool body may ask; this one
/// asks once). Fails in-band when the session has no interactive
/// frontend.
#[rig_tool(
    description = "Ask the user a question and return their answer. Use it when \
                   you need information, a decision, or a confirmation only the \
                   user can provide; do not guess on their behalf. The answer \
                   text is returned verbatim; a dismissed question says so."
)]
pub async fn ask_user(
    #[rig(context)] context: &mut ToolContext,
    question: String,
) -> Result<String, ToolExecutionError> {
    use rig_agent::tool::interaction::UserInteraction;
    let Some(interaction) = context.get::<std::sync::Arc<dyn UserInteraction>>() else {
        return Err(ToolExecutionError::other(
            "this session has no interactive frontend — there is no user to ask; state that and continue with what you have",
        ));
    };
    // An ordinary template consumer: select_any in its zero-option
    // free-text shape (the old ask's degenerate form), payload opaque
    // to the core.
    let payload = serde_json::to_value(tabit_protocol::templates::SelectAnyCard {
        title: "Question".to_string(),
        body: question,
        options: Vec::new(),
        free_text: true,
    })
    .map_err(|error| ToolExecutionError::other(error.to_string()))?;
    Ok(
        match interaction
            .request(tabit_protocol::templates::ui::SELECT_ANY, payload)
            .await
        {
            rig_agent::tool::interaction::InteractionOutcome::Answered(payload) => {
                match serde_json::from_value::<tabit_protocol::templates::SelectAnswer>(payload) {
                    Ok(answer) => answer
                        .text
                        .unwrap_or_else(|| "the user submitted an empty answer".to_string()),
                    Err(_) => "the user's answer could not be read".to_string(),
                }
            }
            rig_agent::tool::interaction::InteractionOutcome::Dismissed => {
                "the user dismissed the question without answering".to_string()
            }
        },
    )
}

/// Run a shell command through bash. On Windows this tool is registered
/// only where a Git-for-Windows install was positively identified at
/// registration ([`shell`]): correctness over coverage — a wrong bash
/// (WSL's launcher, a Cygwin root) is worse than none, so nothing is
/// guessed from a bare `bash.exe` on PATH. Combined output (stdout, then
/// stderr) is capped at 16 KiB — oversized output keeps both ends with
/// the middle omitted and the full text spilled to a file (the spill
/// path rides the notice and `tool_result.details.spill_path`); commands
/// that exceed their timeout, or are cancelled through the run's
/// cancellation token, are killed — process tree included (see the
/// crate-level cancellation contract).
#[rig_tool(description = "Run a shell command and return its combined output. \
                   Commands run through bash (POSIX syntax; on Windows this is \
                   Git Bash). Non-zero exits report the exit code. Output is \
                   capped at 16 KiB; oversized output saves to a file. \
                   Commands time out after 30 seconds unless timeout_secs \
                   says otherwise.")]
pub async fn bash(
    #[rig(context)] context: &mut ToolContext,
    command: String,
    timeout_secs: Option<u64>,
) -> Result<rig_core::OneOrMany<rig_core::message::ToolResultContent>, ToolExecutionError> {
    let interpreter = shell::bash().map_err(ToolExecutionError::other)?;
    run_shell(&interpreter, context, command, timeout_secs).await
}

/// The PowerShell-dialect counterpart of [`bash`], registered on Windows
/// machines with no verified Git Bash — the model always gets a shell
/// whose dialect matches the tool's description.
#[cfg(windows)]
#[rig_tool(description = "Run a shell command and return its combined output. \
                   Commands run through Windows PowerShell — write PowerShell \
                   syntax (Get-ChildItem, $env:NAME, Select-String, ...). \
                   Non-zero exits report the exit code. Output is capped at \
                   16 KiB; oversized output saves to a file. Commands time \
                   out after 30 seconds unless timeout_secs says otherwise.")]
pub async fn powershell(
    #[rig(context)] context: &mut ToolContext,
    command: String,
    timeout_secs: Option<u64>,
) -> Result<rig_core::OneOrMany<rig_core::message::ToolResultContent>, ToolExecutionError> {
    run_shell(&shell::powershell(), context, command, timeout_secs).await
}

/// The shell tool this machine registers: `bash` where a Git-for-Windows
/// install is positively identified (probe-verified absolute `bash.exe`),
/// `powershell` otherwise on Windows, `bash` on Unix. One decision site —
/// the assembly never branches on the platform's shell itself.
pub fn shell_tool() -> DynamicTool {
    #[cfg(windows)]
    return match shell::resolved() {
        shell::Shell::Bash(_) => dynamic_contextual(Bash),
        shell::Shell::Powershell => dynamic_contextual(Powershell),
    };
    #[cfg(not(windows))]
    return dynamic_contextual(Bash);
}

/// The shared execution core of the shell tools: spawn under the resolved
/// interpreter, tree-kill discipline, both deadlines (command timeout +
/// cancellation token), combined output, cap. A truncated success is
/// multi-part: the visible report text plus the structured details (the
/// spill path and the counts) for `tool_result.details`.
async fn run_shell(
    interpreter: &shell::Interpreter,
    context: &mut ToolContext,
    command: String,
    timeout_secs: Option<u64>,
) -> Result<rig_core::OneOrMany<rig_core::message::ToolResultContent>, ToolExecutionError> {
    let timeout = Duration::from_secs(
        timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS),
    );
    // A pre-cancelled token refuses before spawning: "the command never
    // ran" is structural, not a race against a fast command.
    if context
        .get::<tokio_util::sync::CancellationToken>()
        .is_some_and(|token| token.is_cancelled())
    {
        return Err(ToolExecutionError::other(
            "command was interrupted before starting — it did not run".to_string(),
        ));
    }
    let mut wrapped = process_wrap::std::CommandWrap::with_new(&interpreter.argv0, |cmd| {
        cmd.args(interpreter.args)
            .arg(&command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());
        // A session-scoped run works from the session's directory (the
        // subagent ruling); absent the capability the child inherits
        // the process cwd — today's semantics.
        if let Some(cwd) = session_cwd(context) {
            cmd.current_dir(cwd);
        }
    });
    // Tree kill: the process group dies with its leader on Unix; the job
    // object takes the whole tree down on Windows. The drop guard below is
    // the drop-without-cancel backstop (the tokio-only KillOnDrop shim is
    // not available in the std flavor).
    #[cfg(unix)]
    wrapped.wrap(process_wrap::std::ProcessGroup::leader());
    #[cfg(windows)]
    wrapped.wrap(process_wrap::std::JobObject);
    let mut child = wrapped.spawn().map_err(|e| {
        ToolExecutionError::other(format!("cannot start `{}`: {e}", interpreter.argv0))
    })?;

    // Both pipes up front: a full undrained pipe would block the child
    // while the other stream is still being read.
    // The pipes were configured on the command; a missing one means the
    // wrapper dropped them — surface it as an external error, not a panic.
    let stdout_pipe = child
        .stdout()
        .take()
        .ok_or_else(|| ToolExecutionError::other("stdout pipe missing after spawn"))?;
    let stderr_pipe = child
        .stderr()
        .take()
        .ok_or_else(|| ToolExecutionError::other("stderr pipe missing after spawn"))?;
    let stdout_reader = spawn_reader(Some(stdout_pipe));
    let stderr_reader = spawn_reader(Some(stderr_pipe));
    let output = run_with_deadlines(
        child,
        timeout,
        context.get::<tokio_util::sync::CancellationToken>(),
        &interpreter.argv0,
        stdout_reader,
        stderr_reader,
    )?;
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.stderr.is_empty() {
        combined.push_str("\n--- stderr ---\n");
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    // Keep both ends (context starts at the top, results end at the
    // bottom — the first/last-lines mechanism, owner ruling); the
    // middle is dropped, so spill the full output to a temp file —
    // the dropped bytes are unrecoverable without re-running the
    // command, which may be slow or side-effecting. Spill files are
    // never deleted by us: the path was promised to the model; OS
    // temp hygiene owns it.
    let trunc = truncate::truncate_head_tail(&combined);
    let mut visible = trunc.content;
    let mut details = None;
    if trunc.truncated {
        let spill = spill_full_output(&combined)?;
        let detail = if trunc.single_line_split {
            // One line over the whole budget (minified output): say
            // that, not "1 of 1 lines".
            format!("one line of {} bytes, showing both ends", trunc.total_bytes)
        } else {
            format!(
                "showing {} of {} lines from both ends ({} of {} bytes)",
                trunc.output_lines, trunc.total_lines, trunc.output_bytes, trunc.total_bytes
            )
        };
        visible.push_str(&format!("\n\n[{detail}. Full output: {}]", spill.display()));
        // The same facts the notice carries, structured: details.spill_path
        // is enough for the frontend — it reads or displays the spill
        // file itself (owner ruling).
        details = Some(serde_json::json!({
            "truncated": true,
            "output_lines": trunc.output_lines,
            "total_lines": trunc.total_lines,
            "omitted_lines": trunc.total_lines - trunc.output_lines,
            "total_bytes": trunc.total_bytes,
            "spill_path": spill.display().to_string(),
        }));
    }
    if output.status.success() {
        report_with_details(visible, details)
    } else {
        // The exit status rides structure too (the protocol's
        // `failed { exit_code }`): numeric codes pass through as
        // numbers-in-text, so the frontend colors the row without
        // parsing the prose. Signal kills have no code — the prose
        // already says "abnormal termination".
        let mut error = ToolExecutionError::other(format!(
            "command exited with {}:\n{visible}",
            exit_description(&output.status)
        ));
        if let Some(code) = output.status.code() {
            error = error.with_code(code.to_string());
        }
        Err(error)
    }
}

/// Write oversized command output to a temp file. Ids are pid + a
/// monotonic counter — unique per process, no randomness dependency.
fn spill_full_output(full: &str) -> Result<std::path::PathBuf, ToolExecutionError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SPILL_COUNT: AtomicU64 = AtomicU64::new(0);
    let n = SPILL_COUNT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("tabit-bash-{}-{n}.log", std::process::id()));
    std::fs::write(&path, full).map_err(|e| {
        ToolExecutionError::other(format!(
            "cannot save full output to {}: {e}",
            path.display()
        ))
    })?;
    Ok(path)
}

struct CapturedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn exit_description(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("status {code}"),
        None => "an abnormal termination signal".to_string(),
    }
}

/// Kills the process tree when dropped while armed — the std-flavor
/// stand-in for process-wrap's tokio-only `KillOnDrop`: if the tool's
/// future is dropped mid-run, the child must not outlive it. Disarmed on
/// every path that observes the exit.
struct TreeKillGuard {
    child: Box<dyn process_wrap::std::ChildWrapper + Send + Sync>,
    armed: bool,
}

impl TreeKillGuard {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Force-kill the tree and reap it.
    fn kill_tree(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TreeKillGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill_tree();
        }
    }
}

/// Wait for the child under both deadlines — the command timeout and the
/// run's cancellation token — force-killing the process *tree* when either
/// fires (no grace period: a killed command's partial effects are reported
/// as an interruption, not cleaned up). Piped output is captured by the
/// reader threads handed in by the caller; the guard's Drop is the
/// cancel-without-poll backstop.
///
/// Hand-rolled over the wrapper's `try_wait` on purpose: this crate is
/// sync, `tokio::process` would drag a runtime into a std-only tool crate,
/// and the poll loop below is the entire algorithm. The kill itself is
/// `process-wrap`'s (`killpg` on Unix, job-object termination on Windows).
fn run_with_deadlines(
    child: Box<dyn process_wrap::std::ChildWrapper + Send + Sync>,
    timeout: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    argv0: &str,
    stdout_reader: ReaderJoin,
    stderr_reader: ReaderJoin,
) -> Result<CapturedOutput, ToolExecutionError> {
    let mut guard = TreeKillGuard { child, armed: true };
    let deadline = Instant::now() + timeout;
    let status = loop {
        if cancel.is_some_and(|token| token.is_cancelled()) {
            guard.kill_tree();
            return Err(ToolExecutionError::other(
                "command was interrupted before completing — its effects may be                  partial; check before relying on anything it wrote"
                    .to_string(),
            ));
        }
        match guard.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    guard.kill_tree();
                    return Err(ToolExecutionError::other(format!(
                        "command exceeded its {}s timeout and was killed                          (raise timeout_secs if it legitimately needs longer)",
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                return Err(ToolExecutionError::other(format!(
                    "waiting for `{argv0}`: {e}"
                )));
            }
        }
    };
    guard.armed = false;
    Ok(CapturedOutput {
        status,
        stdout: join_reader(stdout_reader),
        stderr: join_reader(stderr_reader),
    })
}

type ReaderJoin = Option<std::thread::JoinHandle<Vec<u8>>>;

fn spawn_reader<R>(pipe: Option<R>) -> ReaderJoin
where
    R: std::io::Read + Send + 'static,
{
    pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            // A failed read still returns what arrived; the tool output
            // cap handles size, and the process exit explains the rest.
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            buf
        })
    })
}

fn join_reader(handle: ReaderJoin) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

/// Erase a contextual `#[rig_tool]` (one taking `#[rig(context)]
/// &mut ToolContext`) into a [`DynamicTool`] so sessions (which rebuild
/// their agent on model switches) can hold mixed tool sets in one
/// vector. Every tabit tool is contextual — they read the session cwd
/// and the run token from the per-run context.
pub fn dynamic_contextual<T>(tool: T) -> DynamicTool
where
    T: rig_agent::tool::Tool + Send + Sync + 'static,
{
    // One shared instance per call site; contextual tools are stateless
    // (`call` takes `&self`), so no per-call clone is needed.
    let tool = std::sync::Arc::new(tool);
    let name = <T as rig_agent::tool::Tool>::NAME.to_string();
    let description = tool.description();
    let parameters = tool.parameters();
    DynamicTool::new(
        name,
        description,
        parameters,
        move |ctx: &mut ToolContext, args: serde_json::Value| {
            let tool = tool.clone();
            Box::pin(async move {
                let typed: <T as rig_agent::tool::Tool>::Args = serde_json::from_value(args)
                    .map_err(|e| ToolExecutionError::other(format!("invalid arguments: {e}")))?;
                let output = <T as rig_agent::tool::Tool>::call(tool.as_ref(), ctx, typed)
                    .await
                    .map_err(|e| tool.map_error(e))?;
                output.into_tool_output()
            })
        },
    )
}

#[cfg(test)]
mod tests;
