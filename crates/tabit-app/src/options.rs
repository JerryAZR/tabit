//! The assembly's options — the library's own input shape. The
//! `tabit-core` binary parses argv into its CLI struct and converts
//! ([`AppOptions`] carries the assembly fields only: what mounts,
//! what filters it, where the world's roots are); an embedder
//! constructs this directly and never meets a command line.

use std::path::PathBuf;

use tabit_config::TabitConfig;
use tabit_session::ModelSelection;

/// Resolve a model reference against the config: `provider/model`
/// when the text before the first `/` names a configured provider,
/// otherwise a bare model id that must be unambiguous (see
/// `TabitConfig::resolve_model_ref`).
pub fn parse_model(raw: &str, config: &TabitConfig) -> Result<ModelSelection, String> {
    let (provider, model) = config
        .resolve_model_ref(raw)
        .map_err(|message| format!("--model: {message}"))?;
    Ok(ModelSelection::new(provider, model))
}

/// The assembly's knobs — one struct, all optional except nothing:
/// the defaults are a bare coding-agent session in the process cwd.
/// Field-for-field these are the `tabit-core` flags' assembly subset
/// (the binary converts); the semantics live with their consumers
/// ([`crate::assemble`] and friends).
#[derive(Clone, Debug, Default)]
pub struct AppOptions {
    /// Resume the session stored at this path.
    pub session: Option<PathBuf>,
    /// Resume this project's newest stored session.
    pub continue_newest: bool,
    /// The model reference (`provider/model` or a bare id).
    pub model: Option<String>,
    /// The per-run turn budget.
    pub max_turns: Option<usize>,
    /// The spawning session's id (a child's lineage; the announce
    /// carries it at the source of truth).
    pub parent: Option<String>,
    /// The spawning tool call's correlation id (pairs the child's
    /// announcement with the open `tool_call` event).
    pub parent_call: Option<String>,
    /// The tool allow-list (include-if-it-exists; `None` = no
    /// filter — the usual case).
    pub tools: Option<String>,
    /// The tool deny-list (exclude-if-it-exists; forwards to
    /// subagent children, extended with `subagent`/`followup`).
    pub without: Option<String>,
    /// The in-memory boot: no session file, nothing persists.
    pub ephemeral: bool,
    /// Replaces the default preamble (identity and standing body);
    /// the environment block, AGENTS.md files, and skills catalog
    /// append as usual.
    pub preamble: Option<String>,
    /// The installed-extensions root (default `~/.tabit/extensions`).
    pub extensions: Option<PathBuf>,
}
