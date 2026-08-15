//! System prompt composition — the default tabit preamble (ROADMAP
//! item 3, v1).
//!
//! Policy (decided; see ROADMAP.md and AGENTS.md):
//! - **AGENTS.md only.** No CLAUDE.md or other vendor instruction files.
//! - **No directory walking.** Two candidate locations: the home level
//!   (`~/.tabit/AGENTS.md`, falling back to `~/.agents/AGENTS.md`) and
//!   the current working directory. Subdirectories are the model's job —
//!   the base prompt tells it to check for AGENTS.md as it descends.
//! - **No size cap.** Instruction files are included verbatim.
//! - **Minimal and stable.** A short identity statement, an environment
//!   block (cwd, platform, UTC date), then the instruction files.
//!   Nothing that goes stale within a session: no clock time, and the
//!   date is frozen at build time (overnight staleness is accepted) so a
//!   session's prompt stays byte-stable and the provider's prompt cache
//!   stays valid. Callers build once per process and must not rebuild
//!   mid-session.

use std::io;
use std::path::{Path, PathBuf};

use crate::SessionError;
use crate::ids;

/// The single supported instruction filename.
const INSTRUCTION_FILE: &str = "AGENTS.md";

const BASE_PROMPT: &str = "\
You are tabit, a coding agent running in the user's terminal. You complete \
tasks by reading files, running commands, and editing code.

When working in a subdirectory, check it for an AGENTS.md file with \
additional instructions.
";

/// One discovered instruction file, verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextFile {
    path: PathBuf,
    content: String,
}

/// The home-level candidates in priority order: tabit's own directory
/// first, then the tool-agnostic `.agents` location.
fn home_candidates(home: &Path) -> [PathBuf; 2] {
    [
        home.join(".tabit").join(INSTRUCTION_FILE),
        home.join(".agents").join(INSTRUCTION_FILE),
    ]
}

/// Read an instruction file, treating *absent* as empty but *unreadable*
/// as a loud error — a file the user deliberately placed must not be
/// silently dropped.
fn read_if_present(path: &Path) -> io::Result<Option<ContextFile>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(ContextFile {
            path: path.to_path_buf(),
            content,
        })),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(source),
    }
}

/// Discover the instruction files: home first (`.tabit` candidate, then
/// the `.agents` fallback — the first one that exists wins), then cwd.
/// The cwd file comes last so the closest instructions get the last word.
fn discover_context_files(home: &Path, cwd: &Path) -> Result<Vec<ContextFile>, SessionError> {
    let mut files = Vec::new();
    for candidate in home_candidates(home) {
        match read_if_present(&candidate) {
            Ok(file) => {
                if let Some(file) = file {
                    files.push(file);
                    break;
                }
            }
            Err(source) => {
                return Err(SessionError::Io {
                    path: candidate,
                    source,
                });
            }
        }
    }
    let cwd_file = cwd.join(INSTRUCTION_FILE);
    match read_if_present(&cwd_file) {
        Ok(Some(file)) => files.push(file),
        Ok(None) => {}
        Err(source) => {
            return Err(SessionError::Io {
                path: cwd_file,
                source,
            });
        }
    }
    Ok(files)
}

/// `\` → `/` so paths read the same on every platform (and survive
/// downstream quoting).
fn normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Assemble the prompt: base identity, environment block, then each
/// instruction file wrapped with its path.
fn compose_system_prompt(cwd: &Path, date: &str, files: &[ContextFile]) -> String {
    let mut prompt = String::new();
    prompt.push_str(BASE_PROMPT);
    prompt.push_str("\n<environment_context>\n");
    prompt.push_str("cwd: ");
    prompt.push_str(&normalize(cwd));
    prompt.push('\n');
    prompt.push_str("platform: ");
    prompt.push_str(std::env::consts::OS);
    prompt.push('\n');
    prompt.push_str("date: ");
    prompt.push_str(date);
    prompt.push_str(" (UTC)\n");
    prompt.push_str("</environment_context>\n");
    if !files.is_empty() {
        prompt.push_str("\n<project_context>\n");
        for file in files {
            prompt.push_str("<file path=\"");
            prompt.push_str(&normalize(&file.path));
            prompt.push_str("\">\n");
            prompt.push_str(&file.content);
            prompt.push_str("\n</file>\n");
        }
        prompt.push_str("</project_context>\n");
    }
    prompt
}

/// The UTC calendar date (`YYYY-MM-DD`) — the only timestamp the prompt
/// carries. humantime's RFC 3339 output is a fixed-width `YYYY-MM-DDT…`
/// prefix; `utc_date_is_bare_ymd` pins that contract.
fn utc_date() -> String {
    ids::now_rfc3339().chars().take(10).collect()
}

/// Build the default tabit system prompt for a process running in `cwd`.
///
/// Reads the home-level and cwd AGENTS.md (see the module docs for the
/// discovery policy). Missing files are fine; a file that exists but
/// cannot be read fails loudly. Build once per process and reuse the
/// string for the session's lifetime.
pub fn build_system_prompt(cwd: &Path) -> Result<String, SessionError> {
    build_with_home(tabit_config::home_dir(), cwd)
}

fn build_with_home(home: Option<PathBuf>, cwd: &Path) -> Result<String, SessionError> {
    let home = home.ok_or_else(|| SessionError::Config {
        message: "a home directory to read AGENTS.md from (neither \
                  USERPROFILE nor HOME is set)"
            .to_string(),
    })?;
    let files = discover_context_files(&home, cwd)?;
    Ok(compose_system_prompt(cwd, &utc_date(), &files))
}

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod tests;
