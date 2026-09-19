//! The host-service capability extension envelopes dispatch to
//! (checklist task 5): verb zero is the ask (the hub's existing
//! lift), verb one is `model_prompt` — one bare model completion,
//! capped, complete-only, usage billed to the session's ledger under
//! the calling extension's name.
//!
//! One capability per run, inserted into the tool context beside the
//! hub capability (hooks and tools see one set); the binary's proxies
//! and hook forwarders lift it onto every pipe call, and the
//! supervisor's pending table holds it while the call runs. The
//! selection is the run's snapshot — a mid-run model switch reaches
//! the next run's requests, like every other per-open snapshot.
//!
//! `model_prompt` is a BARE completion: no session preamble, no
//! tools, no history — its own standalone conversation, so nothing
//! it does can touch the session's context or its prompt cache
//! identity (it gets its own cache key, the subagent ruling applied
//! to extensions: a divergent suffix on the session's route buys
//! nothing and concentrates misses).

use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::future::BoxFuture;
use rig_agent::agent::{Agent, MultiTurnStreamItem};
use rig_agent::streaming::StreamingPrompt as _;
use rig_agent::tool::interaction::{InteractionOutcome, UserInteraction};
use rig_agent::tool::services::{HostServices, ModelPromptOk, ModelPromptRequest, ServiceUsage};
use rig_core::completion::Usage;
use serde_json::Value;
use tabit_config::TabitConfig;
use tabit_protocol::ModelSelection;

use crate::session::ModelFactory;
use crate::session::assemble::build_agent;
use crate::stats::UsageLedger;

/// The hard output cap for extension completions (a dial): they are
/// short by design — titles, summaries, classifications — and a
/// runaway one must not burn the session's budget. Callers may ask
/// lower; nobody asks higher.
const MODEL_PROMPT_MAX_TOKENS: u64 = 4096;

/// The per-run capability. `interaction` is optional: a
/// non-interactive session still serves `model_prompt` (it needs no
/// user) while its asks answer dismissed — fail closed, the askers'
/// contract.
pub struct ExtensionServices {
    interaction: Option<Arc<dyn UserInteraction>>,
    model_factory: ModelFactory,
    config: Arc<TabitConfig>,
    selection: ModelSelection,
    /// The session's live ledger — shared with the session itself, so
    /// extension spend is part of the session's totals the moment it
    /// happens. Not persisted (no log entry carries it): a reload
    /// counts the turns' usage, not the extensions' — recorded as a
    /// v1 gap.
    ledger: Arc<Mutex<UsageLedger>>,
}

impl ExtensionServices {
    pub fn new(
        interaction: Option<Arc<dyn UserInteraction>>,
        model_factory: ModelFactory,
        config: Arc<TabitConfig>,
        selection: ModelSelection,
        ledger: Arc<Mutex<UsageLedger>>,
    ) -> Self {
        Self {
            interaction,
            model_factory,
            config,
            selection,
            ledger,
        }
    }
}

impl HostServices for ExtensionServices {
    fn ask(&self, ui_type: &str, payload: Value) -> BoxFuture<'static, InteractionOutcome> {
        match self.interaction.clone() {
            Some(interaction) => interaction.request(ui_type, payload),
            None => Box::pin(async move { InteractionOutcome::Dismissed }),
        }
    }

    fn model_prompt(
        &self,
        caller: &str,
        request: ModelPromptRequest,
    ) -> BoxFuture<'static, Result<ModelPromptOk, String>> {
        let factory = self.model_factory.clone();
        let config = self.config.clone();
        let fallback_selection = self.selection.clone();
        let ledger = self.ledger.clone();
        let caller = caller.to_string();
        Box::pin(async move {
            let selection = match &request.model {
                Some(reference) => match config.resolve_model_ref(reference) {
                    Ok((provider, model)) => ModelSelection::new(provider, model),
                    Err(message) => {
                        return Err(format!("model reference `{reference}`: {message}"));
                    }
                },
                None => fallback_selection,
            };
            // The extension's own cache route (clamped like every
            // cache key): its prompts share nothing with the
            // session's prefix.
            let cache_key: String = format!("ext-{caller}").chars().take(64).collect();
            let max_tokens = request
                .max_tokens
                .unwrap_or(MODEL_PROMPT_MAX_TOKENS)
                .min(MODEL_PROMPT_MAX_TOKENS);
            let agent: Agent = build_agent(
                &factory,
                &config,
                &selection,
                &cache_key,
                None,
                &[],
                Some(max_tokens),
            )
            .map_err(|error| error.to_string())?;
            // The bare completion: one turn, no tools, its own
            // conversation; collect the final response only.
            let mut stream = agent.stream_prompt(request.prompt).max_turns(1).await;
            let mut text = String::new();
            let mut usage = Usage::default();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                        text = response.output;
                        usage = response.usage;
                    }
                    Ok(_) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            if text.is_empty() && usage.total_tokens == 0 {
                return Err("the completion ended without a response".to_string());
            }
            // Bill: the serving model's row, plus the extension's own
            // tally — spend is visible and attributed. Dollars are the
            // invoice fact (stamped now, never re-derived).
            let cost = crate::model::turn_cost(&config, &selection, &usage);
            tabit_log::lock::lock(&ledger).add_extension(
                &caller,
                &selection.provider,
                &selection.model,
                selection.thinking_level.as_deref(),
                usage,
                cost,
            );
            Ok(ModelPromptOk {
                text,
                usage: ServiceUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    total_tokens: usage.total_tokens,
                },
            })
        })
    }
}

#[cfg(test)]
#[path = "services_tests.rs"]
mod tests;
