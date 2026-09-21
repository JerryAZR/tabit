//! The default permission gate — pi-sanity's core layer ported to
//! Rust (owner ruling 2026-09): a static, heuristic checker that
//! catches careless mistakes while staying out of the way
//! (allow-when-unsure, never a security boundary). The port is
//! **faithful** — same rules, same matching semantics, same decision
//! order, same cd-following and Windows path handling — with one
//! substitution: [brush-parser](https://docs.rs/brush-parser) (MIT,
//! pure Rust, from the brush shell) replaces the `unbash` npm parser;
//! the walker adapts its AST where unbash's node types appear.
//!
//! This crate is the **pure core** (pi-sanity's ARCHITECTURE.md
//! split, kept): no file system, no UI, no tabit types — decisions
//! in, decisions out. The integration (the `AgentHook` member, the
//! `UserInteraction` ask, the settings opt-out) lives in the
//! `tabit-core` binary, which assembles the hook stack.
//!
//! # The porting map (each module ports one pi-sanity file)
//!
//! | pi-sanity `src/`        | this crate                  | responsibility |
//! |-------------------------|-----------------------------|----------------|
//! | `types.ts`              | [`types`] (here)            | `Action`, `CheckResult` |
//! | `bash-walker.ts`        | [`bash_walker`]             | brush-parser AST walk, cd tracking, `FoundCommand` extraction |
//! | `arg-parser.ts`         | [`arg_parser`]              | option/flag/positional matching shapes |
//! | `checker-bash.ts`       | [`checker_bash`]             | command rules against walked commands |
//! | `checker-read.ts`       | [`checker_read`]             | read-path checks |
//! | `checker-write.ts`      | [`checker_write`]            | write-path checks |
//! | `config-loader.ts` / `config-manager.ts` / `config-types.ts` | [`config`] | rules-as-data, the parsed default rule set (v1 ships the defaults only; sources/merging later) |
//! | `generated/default-config.ts` | [`config`]            | the shipped default rule set |
//! | `path-permission.ts`    | [`path_permission`]          | protected-path rules |
//! | `path-utils.ts`         | [`path_utils`]               | normalization, Windows drives, path contexts |
//! | `pre-check.ts`          | [`pre_check`]                | environment pre-checks |
//! | `tmp-rewrite.ts`        | [`tmp_rewrite`]              | temp-path rewriting |
//! | `tool-checker.ts`       | [`tool_checker`]             | the entry point: tool name + args → checks |
//! | `action-utils.ts`       | folded into [`types`]        | stricter-action aggregation |
//!
//! Source of truth: `C:\Users\Jerry\Projects\agent-utils\pi-packages\pi-sanity`
//! (Apache-2.0; the owner's own package, port authorized).
//!
//! # The public API (frozen contract — implementation and tests are
//! written against these signatures; changing one is a coordinated
//! change)
//!
//! ```text
//! pub use types::{Action, CheckResult};
//!
//! pub fn check_tool_call(
//!     tool_name: &str,
//!     input: &serde_json::Map<String, serde_json::Value>,
//!     config: &config::SanityConfig,
//! ) -> Option<CheckResult>;
//!
//! pub fn build_tool_details(
//!     tool_name: &str,
//!     input: &serde_json::Map<String, serde_json::Value>,
//!     config: &config::SanityConfig,
//! ) -> String;
//!
//! // The three checkers, also public (the corpus tests them directly):
//! checker_bash::check_bash(command: &str, config: &config::SanityConfig) -> CheckResult;
//! checker_read::check_read(path: &str, config: &config::SanityConfig) -> CheckResult;
//! checker_write::check_write(path: &str, config: &config::SanityConfig) -> CheckResult;
//!
//! config::SanityConfig        // the parsed rules (default set in v1)
//! config::default_config() -> SanityConfig;
//! bash_walker::walk(script: &str) -> bash_walker::WalkResult;
//! ```
//!
//! Porting discipline (the owner's ruling): **policy and logic
//! unchanged** — port what is there, including pi-sanity's
//! LIMITATIONS.md blind spots (obfuscation, `eval`, `xargs` … are
//! deliberately unparsed-checked); do not redesign, tighten, or
//! "improve" the rules. Deviations are allowed in exactly one place:
//! the parser adapter (unbash node types → `brush_parser::ast`).

pub mod arg_parser;
pub mod bash_walker;
pub mod checker_bash;
pub mod checker_read;
pub mod checker_write;
pub mod config;
pub mod path_permission;
pub mod path_utils;
pub mod pre_check;
pub mod tmp_rewrite;
pub mod tool_checker;
pub mod types;
