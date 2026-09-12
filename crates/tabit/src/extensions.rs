//! Extension assembly (ROADMAP item 9, task 2): turn the
//! supervisor's resolved reports into the model-facing toolset —
//! flat names, one name one tool — plus the wire catalog with
//! provenance and the load-time conflict reports.
//!
//! The rules (EXTENSIONS.md's naming ruling): the model sees the
//! declared name only; an extension tool **replaces** a core tool of
//! the same name and the replacement is reported (mandatory signal,
//! frontend loudness is the frontend's call); an extension tool that
//! collides with another extension's is refused, naming the incumbent
//! — registration order is the scan's (alphabetical), so which one is
//! the newcomer is deterministic, and the user resolves by disabling
//! one. The engine receives a conflict-free set by construction; its
//! duplicate-name shadowing never engages.
//!
//! The proxy tools are thin: the call crosses the pipe, the result
//! maps to the engine's two-part shape (`content_parts`), the
//! session's interaction capability rides along as the ask lane.
//! Cancellation is inherited, not extended: dropping the proxy's
//! future detaches (a late result lands on a closed channel and is
//! discarded); the pipe lane drains on death so nothing hangs.

use std::sync::Arc;

use rig_agent::agent::hook::{ToolCallAction, ToolResultAction};
use rig_agent::agent::{HookStack, on};
use rig_agent::tool::interaction::UserInteraction;
use rig_agent::tool::{DynamicTool, ToolContext};
use rig_core::tool::{ToolExecutionError, content_parts};
use tabit_ext::protocol::{HookDecision, ToolDecl};
use tabit_ext::supervisor::{ExtensionHandle, ExtensionReport, Status, Supervisor};
use tabit_protocol::{
    AvailableExtension, AvailableExtensionTool, ExtensionConflict, ExtensionConflictKind,
    ExtensionsCatalog,
};

/// The boot's mounted extension surface: the proxy tools, the hook
/// stack (forwarding closures over the pipes), the replacements the
/// tools caused (the core tools to unmount), and the catalog snapshot
/// announced once at startup.
pub struct Mounted {
    /// The host, held for the backend's life — dropping the mount is
    /// the host's close (the closing token every extension selects
    /// on). Never read: the ownership IS the function.
    #[allow(dead_code)]
    supervisor: Arc<Supervisor>,
    /// The proxy tools — conflict-free by construction.
    tools: Vec<DynamicTool>,
    /// The forwarded hooks, registered in scan order (any future core
    /// stack composes through `HookStack::merge` — one priority law).
    hooks: HookStack,
    /// The catalog the backend announces (`extensions_available`).
    pub catalog: ExtensionsCatalog,
}

/// What an enabled package contributes beyond the pipe's tools and
/// hooks (item 9, task 4): the skill names its `skills/` directory
/// ships and the provider ids its `providers.toml` fragment landed in
/// the merged config. Built by the boot, consumed by the catalog —
/// provenance, per EXTENSIONS.md, so a frontend can attribute without
/// the model ever seeing a prefix.
#[derive(Debug, Default, Clone)]
pub struct Contributions {
    pub skills: Vec<String>,
    pub providers: Vec<String>,
}

impl Mounted {
    /// The empty mount (print mode and extension-less hosts): one
    /// shape for every assembly.
    pub fn none() -> Mounted {
        Mounted {
            supervisor: std::sync::Arc::new(Supervisor::empty()),
            tools: Vec::new(),
            hooks: HookStack::new(),
            catalog: ExtensionsCatalog::default(),
        }
    }

    /// Assemble from the supervisor's **resolved** reports (call
    /// [`Supervisor::await_resolved`] first — the boot order that
    /// guarantees tools exist at session build). `contributions`
    /// carries the scan-level facts the catalog attributes (skills,
    /// provider fragments), keyed by extension name.
    #[allow(clippy::unreachable)] // the sanctioned crash below (AGENTS.md doctrine)
    pub fn mount(
        supervisor: Arc<Supervisor>,
        core: &[DynamicTool],
        contributions: &std::collections::HashMap<String, Contributions>,
    ) -> Mounted {
        let core_names: Vec<&str> = core.iter().map(|tool| tool.name()).collect();
        let reports = supervisor.reports();
        let (planned, catalog) = plan(&reports, &core_names, contributions);
        let tools = planned
            .into_iter()
            .filter_map(|planned| {
                supervisor
                    .extension(&planned.extension)
                    .map(|handle| proxy(handle, planned.extension, planned.decl))
            })
            .collect();
        // The hook stack: every declared point of every alive
        // extension becomes a forwarding closure, registered in scan
        // order — deterministic, the registration the engine's order
        // law consumes.
        let mut hooks = HookStack::new();
        for report in &reports {
            if !matches!(report.status, Status::Alive) {
                continue;
            }
            for declared in &report.hooks {
                let Some(handle) = supervisor.extension(&report.name) else {
                    continue;
                };
                let spec = (report.name.as_str(), 0);
                hooks = match declared.event.as_str() {
                    "tool_call" => hooks.hook(
                        spec,
                        on::tool_call(move |ctx, call| {
                            forward_tool_call(handle.clone(), ctx, call)
                        }),
                    ),
                    "tool_result" => hooks.hook(
                        spec,
                        on::tool_result(move |ctx, result| {
                            forward_tool_result(handle.clone(), ctx, result)
                        }),
                    ),
                    other => {
                        unreachable!("internal invariant violated: unknown hook point {other}")
                    }
                };
            }
        }
        Mounted {
            supervisor,
            tools,
            hooks,
            catalog,
        }
    }

    /// The proxy tools (clonable — shared across sessions).
    pub fn tools(&self) -> &[DynamicTool] {
        &self.tools
    }

    /// The mounted hooks (clonable — shared across sessions like the
    /// tools).
    pub fn hooks(&self) -> HookStack {
        self.hooks.clone()
    }

    /// The names of the core tools this mount replaced — the assembly
    /// unmounts them. Each process (backend or child) resolves its
    /// own mount against its own core set.
    pub fn replaced_core(&self) -> Vec<String> {
        self.catalog
            .conflicts
            .iter()
            .filter(|conflict| conflict.kind == ExtensionConflictKind::ReplacesCore)
            .map(|conflict| conflict.tool.clone())
            .collect()
    }
}

/// One tool that survived the name assembly, bound to its extension.
struct Planned {
    extension: String,
    decl: ToolDecl,
}

/// The name assembly — pure over the reports, so the conflict rules
/// are testable without a single process. Outputs the surviving
/// (extension, declaration) pairs in registration order and the whole
/// catalog.
fn plan(
    reports: &[ExtensionReport],
    core_names: &[&str],
    contributions: &std::collections::HashMap<String, Contributions>,
) -> (Vec<Planned>, ExtensionsCatalog) {
    let mut planned = Vec::new();
    let mut held: Vec<(String, String)> = Vec::new(); // (tool name, extension)
    let mut conflicts = Vec::new();
    for report in reports {
        let alive = matches!(report.status, Status::Alive);
        for decl in &report.tools {
            if let Some((_, incumbent)) = held.iter().find(|(name, _)| name == &decl.name) {
                // Peer collision: the newcomer is refused, the
                // incumbent named (registration order = scan order).
                conflicts.push(ExtensionConflict {
                    kind: ExtensionConflictKind::RefusedPeer,
                    extension: report.name.clone(),
                    tool: decl.name.clone(),
                    incumbent: Some(incumbent.clone()),
                });
                continue;
            }
            if core_names.contains(&decl.name.as_str()) {
                conflicts.push(ExtensionConflict {
                    kind: ExtensionConflictKind::ReplacesCore,
                    extension: report.name.clone(),
                    tool: decl.name.clone(),
                    incumbent: None,
                });
            }
            held.push((decl.name.clone(), report.name.clone()));
            if alive {
                planned.push(Planned {
                    extension: report.name.clone(),
                    decl: decl.clone(),
                });
            }
        }
    }
    let extensions = reports
        .iter()
        .map(|report| {
            let extra = contributions.get(&report.name);
            AvailableExtension {
                name: report.name.clone(),
                version: report.version.clone(),
                description: report.description.clone(),
                dir: report.dir.display().to_string(),
                status: match report.status {
                    Status::Alive => "alive".to_string(),
                    Status::Starting => "starting".to_string(),
                    Status::Dead { .. } => "dead".to_string(),
                },
                reason: match &report.status {
                    Status::Dead { reason } => Some(reason.clone()),
                    _ => None,
                },
                tools: report
                    .tools
                    .iter()
                    .map(|decl| AvailableExtensionTool {
                        name: decl.name.clone(),
                        description: decl.description.clone(),
                    })
                    .collect(),
                hooks: report.hooks.iter().map(|hook| hook.event.clone()).collect(),
                skills: extra.map(|c| c.skills.clone()).unwrap_or_default(),
                providers: extra.map(|c| c.providers.clone()).unwrap_or_default(),
            }
        })
        .collect();
    (
        planned,
        ExtensionsCatalog {
            extensions,
            conflicts,
        },
    )
}

/// One proxy tool: the declared name, description, and schema; the
/// body forwards over the pipe and maps the wire result.
fn proxy(handle: ExtensionHandle, extension: String, decl: ToolDecl) -> DynamicTool {
    let tool = decl.name;
    DynamicTool::new(
        tool.clone(),
        decl.description,
        decl.schema,
        move |context: &mut ToolContext, args: serde_json::Value| {
            let (handle, tool, extension) = (handle.clone(), tool.clone(), extension.clone());
            Box::pin(async move {
                // The ask lane: the session's capability rides along;
                // a non-interactive session answers dismissed (the
                // lane's fail-closed, same as core tools).
                let ask = context.get::<Arc<dyn UserInteraction>>().cloned();
                let result = handle
                    .call(&tool, args, ask)
                    .await
                    .map_err(ToolExecutionError::other)?;
                match result.error {
                    Some(error) => Err(ToolExecutionError::other(format!(
                        "extension `{extension}`: {error}"
                    ))),
                    None => content_parts(result.report, result.details),
                }
            })
        },
    )
}

/// Forward one pre-call hook to an extension. The payload carries
/// what a policy needs: the session identity (per-session state), the
/// tool, the arguments. A dead lane fails **open** — crash isolation:
/// one dead package cannot brick the tool phase, and the death itself
/// is reported loudly (stderr, the catalog's dead standing).
fn forward_tool_call<'a>(
    handle: ExtensionHandle,
    ctx: &'a rig_agent::agent::HookContext,
    call: rig_agent::agent::hook::ToolCall<'a>,
) -> futures::future::BoxFuture<'static, ToolCallAction> {
    let payload = serde_json::json!({
        "session": ctx.session_id(),
        "tool": call.tool_name,
        "args": call.args,
    });
    let ask = ctx.interaction();
    Box::pin(async move {
        match handle.hook("tool_call", payload, ask).await {
            Ok(HookDecision::Skip { message }) => ToolCallAction::skip(message),
            // Run is the neutral answer; Keep on a call point is
            // protocol misuse — treat it as neutral, not fatal.
            Ok(_) => ToolCallAction::run(),
            Err(_) => ToolCallAction::run(),
        }
    })
}

/// Forward one post-result hook: the presentation rides the payload
/// (rendered), and Keep is the only v1 wire decision — result-hook
/// consumers are observers for now.
fn forward_tool_result<'a>(
    handle: ExtensionHandle,
    ctx: &'a rig_agent::agent::HookContext,
    result: rig_agent::agent::hook::ToolResultEvent<'a>,
) -> futures::future::BoxFuture<'static, ToolResultAction> {
    let payload = serde_json::json!({
        "session": ctx.session_id(),
        "tool": result.tool_name,
        "args": result.args,
        "presentation": result.presentation.render(),
    });
    let ask = ctx.interaction();
    Box::pin(async move {
        // Keep either way: the only wire decision, and the fail-open
        // answer for a dead lane.
        let _ = handle.hook("tool_result", payload, ask).await;
        ToolResultAction::keep()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabit_ext::protocol::HookDecl;

    fn report(name: &str, tools: &[(&str, &str)], alive: bool) -> ExtensionReport {
        ExtensionReport {
            name: name.to_string(),
            dir: format!("C:/ext/{name}").into(),
            version: "0.1.0".to_string(),
            description: Some("d".to_string()),
            status: if alive {
                Status::Alive
            } else {
                Status::Dead {
                    reason: "no handshake within 30s".to_string(),
                }
            },
            tools: tools
                .iter()
                .map(|(name, description)| ToolDecl {
                    name: name.to_string(),
                    description: description.to_string(),
                    schema: serde_json::json!({"type": "object"}),
                })
                .collect(),
            hooks: vec![HookDecl {
                event: "tool_call".to_string(),
            }],
        }
    }

    #[test]
    fn a_flat_name_assembles_one_tool() {
        let reports = vec![report("echo", &[("echo", "says it back")], true)];
        let (planned, catalog) = plan(&reports, &["read", "bash"], &Default::default());
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].decl.name, "echo");
        assert!(catalog.conflicts.is_empty());
        assert_eq!(catalog.extensions[0].status, "alive");
        assert_eq!(catalog.extensions[0].tools.len(), 1);
        assert_eq!(catalog.extensions[0].hooks, vec!["tool_call".to_string()]);
    }

    #[test]
    fn a_core_name_is_replaced_and_reported() {
        let reports = vec![report("shadow", &[("read", "the shadow read")], true)];
        let (planned, catalog) = plan(&reports, &["read", "bash"], &Default::default());
        assert_eq!(planned.len(), 1, "the shadow mounts");
        assert_eq!(catalog.conflicts.len(), 1);
        assert_eq!(
            catalog.conflicts[0].kind,
            ExtensionConflictKind::ReplacesCore
        );
        assert_eq!(catalog.conflicts[0].tool, "read");
        assert_eq!(catalog.conflicts[0].extension, "shadow");
    }

    #[test]
    fn a_peer_collision_refuses_the_newcomer_and_names_the_incumbent() {
        // Scan order is alphabetical: clash-a registers first.
        let reports = vec![
            report("clash-a", &[("clashy", "the incumbent")], true),
            report("clash-b", &[("clashy", "the newcomer")], true),
        ];
        let (planned, catalog) = plan(&reports, &[], &Default::default());
        assert_eq!(planned.len(), 1, "one name, one tool");
        assert_eq!(planned[0].extension, "clash-a");
        assert_eq!(catalog.conflicts.len(), 1);
        let conflict = &catalog.conflicts[0];
        assert_eq!(conflict.kind, ExtensionConflictKind::RefusedPeer);
        assert_eq!(conflict.extension, "clash-b");
        assert_eq!(conflict.incumbent.as_deref(), Some("clash-a"));
    }

    #[test]
    fn a_dead_extension_lists_but_mounts_nothing() {
        let reports = vec![report("gone", &[("tool", "declared once")], false)];
        let (planned, catalog) = plan(&reports, &[], &Default::default());
        assert!(planned.is_empty());
        assert_eq!(catalog.extensions[0].status, "dead");
        assert_eq!(
            catalog.extensions[0].reason.as_deref(),
            Some("no handshake within 30s")
        );
        // The dead declaration still lists in the catalog — what it
        // would have served is the report.
        assert_eq!(catalog.extensions[0].tools.len(), 1);
    }
}
