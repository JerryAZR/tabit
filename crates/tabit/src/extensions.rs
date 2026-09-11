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

use rig_agent::tool::interaction::UserInteraction;
use rig_agent::tool::{DynamicTool, ToolContext};
use rig_core::tool::{ToolExecutionError, content_parts};
use tabit_ext::protocol::ToolDecl;
use tabit_ext::supervisor::{ExtensionHandle, ExtensionReport, Status, Supervisor};
use tabit_protocol::{
    AvailableExtension, AvailableExtensionTool, ExtensionConflict, ExtensionConflictKind,
    ExtensionsCatalog,
};

/// The boot's mounted extension surface: the proxy tools, the
/// replacements they caused (the core tools to unmount), and the
/// catalog snapshot announced once at startup.
pub struct Mounted {
    /// The host, held for the backend's life — dropping the mount is
    /// the host's close (the closing token every extension selects
    /// on). Never read: the ownership IS the function.
    #[allow(dead_code)]
    supervisor: Arc<Supervisor>,
    /// The proxy tools — conflict-free by construction.
    tools: Vec<DynamicTool>,
    /// The catalog the backend announces (`extensions_available`).
    pub catalog: ExtensionsCatalog,
}

impl Mounted {
    /// The empty mount (print mode and extension-less hosts): one
    /// shape for every assembly.
    pub fn none() -> Mounted {
        Mounted {
            supervisor: std::sync::Arc::new(Supervisor::empty()),
            tools: Vec::new(),
            catalog: ExtensionsCatalog::default(),
        }
    }

    /// Assemble from the supervisor's **resolved** reports (call
    /// [`Supervisor::await_resolved`] first — the boot order that
    /// guarantees tools exist at session build).
    pub fn mount(supervisor: Arc<Supervisor>, core: &[DynamicTool]) -> Mounted {
        let core_names: Vec<&str> = core.iter().map(|tool| tool.name()).collect();
        let reports = supervisor.reports();
        let (planned, catalog) = plan(&reports, &core_names);
        let tools = planned
            .into_iter()
            .filter_map(|planned| {
                supervisor
                    .extension(&planned.extension)
                    .map(|handle| proxy(handle, planned.extension, planned.decl))
            })
            .collect();
        Mounted {
            supervisor,
            tools,
            catalog,
        }
    }

    /// The proxy tools (clonable — shared across sessions).
    pub fn tools(&self) -> &[DynamicTool] {
        &self.tools
    }

    /// The names of the core tools this mount replaced — the assembly
    /// unmounts them (children keep the core: they never boot
    /// extensions, the leaf law).
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
fn plan(reports: &[ExtensionReport], core_names: &[&str]) -> (Vec<Planned>, ExtensionsCatalog) {
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
        .map(|report| AvailableExtension {
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
        let (planned, catalog) = plan(&reports, &["read", "bash"]);
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
        let (planned, catalog) = plan(&reports, &["read", "bash"]);
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
        let (planned, catalog) = plan(&reports, &[]);
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
        let (planned, catalog) = plan(&reports, &[]);
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
