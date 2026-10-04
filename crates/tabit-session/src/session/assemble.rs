//! Agent construction: the freshness-checked cache (`ensure_agent`) and
//! the pure derivation it shares with the session's own assembly.

use super::builder::{ModelFactory, SessionBuilder};
use super::mailbox::Mailbox;
use super::{Session, SharedConversation};
use crate::context_manager::ContextManager;
use crate::error::SessionError;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tabit_config::TabitConfig;
use tabit_engine::agent::{Agent, AgentBuilder};
use tabit_engine::tool::DynamicTool;
use tabit_protocol::ModelSelection;
use tokio_util::sync::CancellationToken;

impl Session {
    /// The agent-cache freshness check — the point-of-use half of the
    /// selection-is-truth rule ([`Session::set_model`] is the write
    /// half). Any future writer that swaps `selection` (config reload,
    /// say) cannot leave a stale agent serving requests, because the
    /// one reader derives rather than trusts. Returns the agent and
    /// the selection it is built for — the run's snapshot pair. A
    /// selection-less session (`None` — the zero-config boot) cannot
    /// open a run at all: the teaching failure, carried as
    /// `run_failed { kind: model }` by the caller.
    pub(super) fn ensure_agent(&mut self) -> Result<(Arc<Agent>, ModelSelection), SessionError> {
        let Some(selection) = self.selection() else {
            // The teaching failure — carried as `run_failed { kind:
            // model }` by the caller. It branches on the same
            // predicate as the boot's note (the catalog's two halves,
            // FRONTEND.md §3.1): no providers at all means `login`
            // has nothing to validate against — write providers.toml
            // and restart; config exists but nothing is usable —
            // `login` fixes it in-app, then `model` lands the
            // selection.
            let message = if crate::lock::lock(&self.world).config.providers.is_empty() {
                "no model selected — this backend has no providers.toml at all (the normal \
                 fresh-install state); create ~/.tabit/providers.toml and restart the backend"
                    .to_string()
            } else {
                "no model selected — every configured provider lacks a key; add one with the \
                 `login` command (no restart), then switch with the `model` command"
                    .to_string()
            };
            return Err(SessionError::Config { message });
        };
        // The pair is written together, so a matching stamp means the
        // agent stands. The guard is held across the (sync) build: a
        // refresh racing in between clears the cache, and the next
        // open rebuilds — no stale build survives a refresh.
        let mut world = crate::lock::lock(&self.world);
        if let Some((agent, built_for)) = &world.agent
            && *built_for == selection
        {
            return Ok((agent.clone(), selection));
        }
        let agent = Arc::new(build_agent(
            &world.factory,
            &world.config,
            &selection,
            &self.id,
            self.preamble.as_deref(),
            &self.tools,
            None,
        )?);
        world.agent = Some((agent.clone(), selection.clone()));
        Ok((agent, selection))
    }

    pub(super) fn assemble(
        builder: SessionBuilder,
        buffer: crate::writer::SharedBuffer,
        path: Option<PathBuf>,
        id: String,
        cwd: PathBuf,
        resumed: bool,
    ) -> Result<Self, SessionError> {
        if builder.max_turns == 0 {
            // The engine's entry contract (ENGINE.md): every outer loop
            // runs at least one turn. Rejected here — at the builder —
            // because a zero budget would otherwise fail every run
            // before its engine could drain, and the session cannot be
            // built to run at all.
            return Err(SessionError::Config {
                message: "max_turns must be at least 1 — every outer loop runs at least one turn"
                    .to_string(),
            });
        }
        let selection_cell = Arc::new(Mutex::new(builder.selection.clone()));
        let conversation_cell: Arc<std::sync::RwLock<ContextManager>> = Arc::new(
            std::sync::RwLock::new(ContextManager::empty(buffer.clone())),
        );
        let shared_conversation = SharedConversation {
            conversation: conversation_cell.clone(),
        };
        // No opening agent: the cache is lazy (the selection-is-truth
        // rule derives it at run open) — a selection-less session has
        // none to build, and a selection that cannot construct in this
        // environment must not kill the boot: its failure is the
        // run-open `run_failed { kind: model }`.
        let session = Self {
            world: super::world::SessionWorld::new(
                builder.config,
                builder.model_factory,
                builder.factory_custom,
            ),
            selection: selection_cell,
            preamble: builder.preamble,
            tools: builder.tools,
            max_turns: builder.max_turns,
            run_hooks: builder.run_hooks,
            conversation: conversation_cell,
            buffer,
            shared_conversation,
            persist_notices: Arc::new(std::sync::OnceLock::new()),
            ledger: std::sync::Arc::new(
                std::sync::Mutex::new(crate::stats::UsageLedger::default()),
            ),
            abort: std::sync::Arc::new(std::sync::Mutex::new(CancellationToken::new())),
            mailbox: Mailbox::default(),
            path,
            cwd,
            id,
            resumed,
            interaction: None,
            subagent_parts: builder.subagent_parts,
            // The session's kept-alive children — always minted (an
            // empty pool is one map; the sweep is a no-op) so the
            // run loop and the per-run capability never face a
            // parts-without-pool invariant.
            subagent_pool: std::sync::Arc::new(crate::subagent_pool::SubagentPool::new()),
            skills: builder.skills,
            event_tap: Arc::new(std::sync::OnceLock::new()),
            compaction: Arc::new(crate::compaction::Compaction::new()),
        };
        // The mailbox's invocation expander rides the same catalog the
        // prompt and the `skill` tool read — one discovery, one table
        // (the skills-available ruling).
        if let Some(skills) = &session.skills {
            session.mailbox.attach_expander(skills.clone());
        }
        Ok(session)
    }
}

/// Build the agent a selection resolves to. Everything except the
/// selection is fixed at assembly (factory, config, preamble, tools),
/// so this is a pure function of its arguments — the derivation the
/// cache check in [`Session::ensure_agent`] and the one-shot
/// `model_prompt` build in `services.rs` share.
pub(crate) fn build_agent(
    model_factory: &ModelFactory,
    config: &TabitConfig,
    selection: &ModelSelection,
    cache_key: &str,
    preamble: Option<&str>,
    tools: &[DynamicTool],
    max_tokens_override: Option<u64>,
) -> Result<Agent, SessionError> {
    let handle = (model_factory)(&selection.provider, &selection.model, cache_key)?;
    let params = crate::registry::request_params(config, selection);
    // `dynamic_tools` (even with an empty vec) moves the builder to
    // its tool-configured state, keeping one concrete type through
    // the preamble/build chain.
    let mut builder = AgentBuilder::new(handle).dynamic_tools(tools.to_vec());
    if let Some(preamble) = preamble {
        builder = builder.preamble(preamble);
    }
    // Configured request parameters are pure forwarding (reviewed
    // 2026-08): the model's knobs, nothing interpreted.
    // The override is the hard cap for extension completions — it
    // wins over any configured value (a runaway `model_prompt` must
    // not burn the session's budget).
    let max_tokens = max_tokens_override.or(params.max_tokens);
    if let Some(max_tokens) = max_tokens {
        builder = builder.max_tokens(max_tokens);
    }
    if let Some(temperature) = params.temperature {
        builder = builder.temperature(temperature);
    }
    // `top_p`/`top_k` have no dedicated field on the completion
    // request — they ride the same flattened `additional_params` map
    // as `extra_body`, which is the compat escape hatch and therefore
    // gets the last word over the named knobs.
    let mut additional = serde_json::Map::new();
    if let Some(top_p) = params.top_p {
        additional.insert("top_p".to_string(), serde_json::json!(top_p));
    }
    if let Some(top_k) = params.top_k {
        additional.insert("top_k".to_string(), serde_json::json!(top_k));
    }
    if let Some(extra) = params.extra_body {
        additional.extend(extra);
    }
    if !additional.is_empty() {
        builder = builder.additional_params(serde_json::Value::Object(additional));
    }
    Ok(builder.build())
}
