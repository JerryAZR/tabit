//! Agent construction: the freshness-checked cache (`ensure_agent`) and
//! the pure derivation it shares with the session's own assembly.

use super::builder::{ModelFactory, SessionBuilder};
use super::mailbox::Mailbox;
use super::{Session, SharedConversation};
use crate::context_manager::ContextManager;
use crate::error::SessionError;
use rig_agent::agent::{Agent, AgentBuilder};
use rig_agent::tool::DynamicTool;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tabit_config::TabitConfig;
use tabit_protocol::ModelSelection;
use tokio_util::sync::CancellationToken;

impl Session {
    /// The agent-cache freshness check — the point-of-use half of the
    /// selection-is-truth rule ([`Session::set_model`] is the write
    /// half). Any future writer that swaps `selection` (config reload,
    /// say) cannot leave a stale agent serving requests, because the
    /// one reader derives rather than trusts.
    pub(super) fn ensure_agent(&mut self) -> Result<(), SessionError> {
        let selection = self.selection();
        if self.agent_built_for == selection {
            return Ok(());
        }
        self.agent = Arc::new(build_agent(
            &self.model_factory,
            &self.config,
            &selection,
            &self.id,
            self.preamble.as_deref(),
            &self.tools,
        )?);
        self.agent_built_for = selection;
        Ok(())
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
        let conversation_cell: Arc<std::sync::RwLock<ContextManager>> = Arc::new(
            std::sync::RwLock::new(ContextManager::empty(buffer.clone())),
        );
        let shared_conversation = SharedConversation {
            conversation: conversation_cell.clone(),
        };
        // The opening agent is derived from the resolved selection before
        // the struct exists (the placeholder this replaces existed only
        // to satisfy the field initializer).
        let agent = Arc::new(build_agent(
            &builder.model_factory,
            &builder.config,
            &builder.selection,
            &id,
            builder.preamble.as_deref(),
            &builder.tools,
        )?);
        let session = Self {
            config: builder.config,
            selection: Arc::new(Mutex::new(builder.selection.clone())),
            preamble: builder.preamble,
            tools: builder.tools,
            max_turns: builder.max_turns,
            model_factory: builder.model_factory,
            run_hooks: builder.run_hooks,
            agent,
            agent_built_for: builder.selection,
            conversation: conversation_cell,
            buffer,
            shared_conversation,
            persist_notices: Arc::new(std::sync::OnceLock::new()),
            ledger: crate::stats::UsageLedger::default(),
            abort: std::sync::Arc::new(std::sync::Mutex::new(CancellationToken::new())),
            mailbox: Mailbox::default(),
            path,
            cwd,
            id,
            resumed,
            interaction: None,
        };
        Ok(session)
    }
}

/// Build the agent a selection resolves to. Everything except the
/// selection is fixed at assembly (factory, config, preamble, tools),
/// so this is a pure function of its arguments — the derivation the
/// cache check in [`Session::ensure_agent`] and the one-shot build in
/// [`Session::assemble`] share.
fn build_agent(
    model_factory: &ModelFactory,
    config: &TabitConfig,
    selection: &ModelSelection,
    cache_key: &str,
    preamble: Option<&str>,
    tools: &[DynamicTool],
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
    if let Some(max_tokens) = params.max_tokens {
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
