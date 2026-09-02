//! Session construction: the builder surface, the model-factory seam,
//! and the two entrances — fresh `create` and log `resume`.

use super::selection::register_record;
use super::{DEFAULT_MAX_TURNS, Session};
use crate::error::SessionError;
use crate::model::validate_selection;
use crate::registry::ModelRegistry;
use crate::store::SessionStore;
use crate::writer::SessionWriter;
use rig_agent::agent::ModelHandle;
use rig_agent::tool::DynamicTool;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};
use tabit_protocol::ModelSelection;

/// Builds a [`Session`], either fresh or resumed from a log.
pub struct SessionBuilder {
    pub(super) store: SessionStore,
    pub(super) config: Arc<TabitConfig>,
    pub(super) selection: ModelSelection,
    pub(super) preamble: Option<String>,
    pub(super) tools: Vec<DynamicTool>,
    pub(super) max_turns: usize,
    pub(super) model_factory: ModelFactory,
    pub(super) run_hooks: Option<rig_agent::agent::HookStack>,
    pub(super) subagent_parts: Option<Arc<crate::subagent::SubagentParts>>,
}

/// Builds the model behind a selection: `(provider, model, cache_key)`
/// to a type-erased handle. The cache key is the session's stable id —
/// a provider-neutral prompt-cache routing hint; providers with no
/// such knob ignore it. Overridable for callers that construct models
/// themselves (and for tests).
pub type ModelFactory =
    Arc<dyn Fn(&str, &str, &str) -> Result<ModelHandle, SessionError> + Send + Sync>;

/// What happened while resuming a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResumeReport {
    /// The model selection the session resumed with (from the last
    /// `model_change` record, if any).
    pub resumed_model: Option<ModelSelection>,
}

impl SessionBuilder {
    /// Start building a session that will use `selection`. The selection is
    /// validated against the config immediately.
    ///
    /// The default model factory mints a **per-builder registry** (its
    /// own provider client caches) — an ergonomic default for
    /// single-session callers. Hosts serving many sessions pass one
    /// shared factory ([`ModelRegistry::factory`]) instead: providers
    /// are user config, process-wide, and so are their connection
    /// pools (owner ruling, PROTOCOL.md v3).
    pub fn new(
        store: SessionStore,
        config: Arc<TabitConfig>,
        auth: Arc<AuthConfig>,
        selection: ModelSelection,
    ) -> Result<Self, SessionError> {
        validate_selection(&selection, &config)?;
        let default_factory: ModelFactory =
            ModelRegistry::new(config.clone(), auth.clone()).factory();
        Ok(Self {
            store,
            config,
            selection,
            preamble: None,
            tools: Vec::new(),
            max_turns: DEFAULT_MAX_TURNS,
            model_factory: default_factory,
            run_hooks: None,
            subagent_parts: None,
        })
    }

    /// Mount subagent support: the process-wide parts every child is
    /// built from (see [`crate::subagent`]). The parts' toolset is the
    /// child toolset — it must exclude the subagent tool itself
    /// (recursion depth is enforced by omission).
    pub fn subagents(mut self, parts: Arc<crate::subagent::SubagentParts>) -> Self {
        self.subagent_parts = Some(parts);
        self
    }

    /// The system preamble hoisted into every request.
    pub fn preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = Some(preamble.into());
        self
    }

    /// Register a runtime-defined tool available to every outer loop.
    pub fn dynamic_tool(mut self, tool: DynamicTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Model-call budget per outer loop.
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Mount a hook stack on every run (the assembly's seam for
    /// dev-time/extension policy — the permission gate). The stack is
    /// a value: build it once, closures capture their own
    /// session-scoped state, and it clones into each run.
    pub fn hooks(mut self, stack: rig_agent::agent::HookStack) -> Self {
        self.run_hooks = Some(stack);
        self
    }

    /// Supply models yourself instead of through tabit config. The factory
    /// receives `(provider, model, cache_key)`; it is consulted on session
    /// creation, on resume, and on every model switch. Takes the named
    /// [`ModelFactory`] handle (cheaply clonable, shareable across
    /// builders) so callers like `ModelRegistry::factory` pass through
    /// unwrapped.
    pub fn model_factory(mut self, factory: ModelFactory) -> Self {
        self.model_factory = factory;
        self
    }

    /// Create a fresh session. Nothing touches the disk: the file (with
    /// the opening model selection recorded right after the header)
    /// materializes at the first user message, so a session that never
    /// runs leaves nothing behind — not a header-only orphan.
    /// Create a fresh session. Nothing touches the disk: the file (with
    /// the opening model selection recorded right after the header)
    /// materializes at the first user message, so a session that never
    /// runs leaves nothing behind — not a header-only orphan.
    pub fn create(self, cwd: &str) -> Result<Session, SessionError> {
        let writer = self.store.create(cwd);
        let selection = self.selection.clone();
        let id = writer.session_id().to_string();
        let path = writer.path().to_path_buf();
        let session = Session::assemble(
            self,
            std::sync::Arc::new(std::sync::Mutex::new(writer)),
            Some(path),
            id,
            PathBuf::from(cwd),
            false,
        )?;
        // The opening model_change enqueues at once: write-behind — it
        // lands with the session's first drain, and a session that
        // never runs materializes nothing (the writer's no-orphan
        // gate). A register write before then supersedes it (last
        // model_change wins).
        crate::lock::lock(&session.buffer).prequeue(&register_record(&selection));
        Ok(session)
    }

    /// Create an **ephemeral** session: in memory only, over the
    /// disk-unplugged [`NullBuffer`](crate::writer::NullBuffer) —
    /// everything folds and grows, nothing persists. No file ever
    /// materializes (there is no orphan to gate), so there is nothing
    /// to resume, replay, or list; the id is still real (the stream
    /// stamp and the prompt-cache key). The subagent scratch child;
    /// also a cheap test session.
    pub fn ephemeral(self, cwd: &str) -> Result<Session, SessionError> {
        let buffer: crate::writer::SharedBuffer =
            std::sync::Arc::new(std::sync::Mutex::new(crate::writer::NullBuffer));
        Session::assemble(
            self,
            buffer,
            None,
            crate::ids::new_session_id(),
            PathBuf::from(cwd),
            false,
        )
    }

    /// Resume the session stored at `path`: parse it once (the tree, the
    /// head, the selection register, the context, the cumulative stats),
    /// adopt the result as the resident state, and continue with the
    /// builder's selection. Callers resolve that selection through
    /// [`ModelRegistry::default_selection`] (explicit choice > the log's
    /// last model > configured preference); when it differs from the
    /// file's last recorded model the switch is recorded as a
    /// `model_change` side record. The register is file-scoped (the
    /// owner ruling): the last model_change in append order wins,
    /// whichever branch the conversation is on.
    pub fn resume(self, path: &Path) -> Result<(Session, ResumeReport), SessionError> {
        let parsed = self.store.open_path(path)?;
        let report = ResumeReport {
            resumed_model: parsed.register.clone(),
        };
        validate_selection(&self.selection, &self.config)?;
        let id = parsed.header.id.clone();
        let file_path = parsed.path.clone();
        let writer = SessionWriter::append_to(&parsed.path, id.clone(), parsed.file_len)?;
        let cwd = PathBuf::from(parsed.header.cwd.clone());
        let mut session = Session::assemble(
            self,
            std::sync::Arc::new(std::sync::Mutex::new(writer)),
            Some(file_path),
            id,
            cwd,
            true,
        )?;
        session.ledger = parsed.stats.clone();
        // The conversation's owner is born from the parsed tree over
        // the session's one buffer (from_tree is the only preloaded
        // entrance; the context is derived, never parsed).
        *crate::lock::write(&session.conversation) =
            crate::context_manager::ContextManager::from_tree(
                parsed.tree.clone(),
                session.buffer.clone(),
            );
        let selection = session.selection();
        let same_model = matches!(
            &report.resumed_model,
            Some(last) if last.provider == selection.provider
                && last.model == selection.model
                && last.thinking_level == selection.thinking_level
        );
        if !same_model {
            // Either a caller-directed switch at resume time, or a log
            // without any model_change yet — either way the session's
            // opening state is durable from here on, through the one
            // register-write site like every other switch.
            session.model_register().write(selection);
        }
        Ok((session, report))
    }
}
