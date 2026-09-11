//! Skills: the agentskills.io format over a four-source discovery
//! ladder, the prompt catalog, and the confined `skill` tool
//! (ROADMAP item 3, rulings 2026-09).
//!
//! A skill is a directory carrying a `SKILL.md` with `---`-delimited
//! YAML frontmatter (`name`, `description`). Discovery (ruled):
//! **merge with override on name collision**, precedence lowest →
//! highest `~/.agents/skills/`, `~/.tabit/skills/`,
//! `<cwd>/.agents/skills/`, `<cwd>/.tabit/skills/` — workspace beats
//! home, and within a level `.tabit` beats `.agents`. Unlike
//! AGENTS.md's single-file fallback, all four dirs merge.
//!
//! Per-directory shape follows the references (pi/yaca): recursive,
//! a directory containing SKILL.md is a leaf, dotdirs are skipped,
//! loose `.md` files are never skills, entries visit in sorted order
//! (a deterministic catalog), and malformed skills skip + warn —
//! never fatal.
//!
//! The model-side surface (ruled, the yaca shape): the catalog rides
//! the system prompt — name, description, location — and the body
//! enters context only on invocation through the [`skill`] tool,
//! which abstracts away that skills are host files: the model
//! expresses intent by name. `rel_path` selects within the named
//! skill's directory and is **confined** to it — lexically first,
//! then symlink-resolved at read time — so a symlink inside the
//! skill dir pointing outside cannot escape.

use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use rig_derive::rig_tool;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One discovered skill. `base_dir` is the canonical directory
/// containing SKILL.md (symlinks resolved at discovery — the tool's
/// confinement compares canonical against canonical); `skill_file` is
/// the SKILL.md inside it. Both absolute.
#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub base_dir: PathBuf,
    pub skill_file: PathBuf,
    /// Which discovery level supplied the entry (the winner, on
    /// collision): home or workspace.
    pub level: SkillLevel,
}

/// The discovery level a skill came from — the wire's `level` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillLevel {
    User,
    Workspace,
}

impl SkillLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Workspace => "workspace",
        }
    }
}

/// The discovered catalog: the merge result of one discovery pass,
/// shared as typed tool context (the `skill` tool's lookup) and the
/// prompt catalog's source. Infallible at the API boundary — a bare
/// machine discovers an empty catalog, and an empty catalog renders
/// no prompt block and lists no skills.
#[derive(Debug, Default, Clone)]
pub struct Skills {
    entries: Vec<SkillEntry>,
}

/// Discover at the home and cwd levels. Home resolution follows the
/// prompt builder's convention (`tabit_config::home_dir`); a missing
/// home simply has no skills.
pub fn discover(cwd: &Path) -> Skills {
    discover_with_home(tabit_config::home_dir().as_deref(), cwd)
}

/// The home-injected core (tests run against tempdirs).
pub fn discover_with_home(home: Option<&Path>, cwd: &Path) -> Skills {
    let mut entries: Vec<SkillEntry> = Vec::new();
    // Scan order IS the collision order: last registration wins, so
    // `<cwd>/.tabit/skills/` — the last scanned — wins overall (the
    // ruled ladder).
    let mut sources: Vec<(PathBuf, SkillLevel)> = Vec::new();
    if let Some(home) = home {
        sources.push((home.join(".agents/skills"), SkillLevel::User));
        sources.push((home.join(".tabit/skills"), SkillLevel::User));
    }
    sources.push((cwd.join(".agents/skills"), SkillLevel::Workspace));
    sources.push((cwd.join(".tabit/skills"), SkillLevel::Workspace));
    for (dir, level) in sources {
        for entry in scan_skills_dir(&dir, level) {
            match entries.iter().position(|e| e.name == entry.name) {
                Some(index) => {
                    if let Some(slot) = entries.get_mut(index) {
                        tracing::warn!(
                            skill = %entry.name,
                            kept = %entry.skill_file.display(),
                            dropped = %slot.skill_file.display(),
                            "skill name collision: last registration wins"
                        );
                        *slot = entry;
                    }
                }
                None => entries.push(entry),
            }
        }
    }
    Skills { entries }
}

/// Recursively scan one skills dir: a directory containing SKILL.md
/// is a leaf, dotdirs are skipped, loose files are never skills, and
/// anything malformed skips with a warn (never fatal). Entries visit
/// in sorted order so the catalog is deterministic.
fn scan_skills_dir(dir: &Path, level: SkillLevel) -> Vec<SkillEntry> {
    let mut out = Vec::new();
    scan_into(dir, level, &mut out);
    out
}

fn scan_into(dir: &Path, level: SkillLevel, out: &mut Vec<SkillEntry>) {
    // A missing skills dir is the normal case on a bare machine —
    // not a diagnostic. Other read failures warn (no silent skips).
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            tracing::warn!(dir = %dir.display(), error = %err, "skipping unreadable skills dir");
            return;
        }
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in read {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(err) => {
                tracing::warn!(dir = %dir.display(), error = %err, "skipping unreadable skills entry");
            }
        }
    }
    paths.sort();
    for path in paths {
        // `metadata` follows symlinks: a symlinked skill dir scans as
        // a dir (resolved at load); a dangling one warns when
        // SKILL.md cannot be read.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue; // loose files are never skills
        }
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue; // dotdirs are skipped
        }
        let skill_file = path.join("SKILL.md");
        if skill_file.is_file() {
            // A dir containing SKILL.md is a LEAF — no deeper recursion.
            if let Some(entry) = load_skill(&skill_file, level) {
                out.push(entry);
            }
        } else {
            scan_into(&path, level, out);
        }
    }
}

/// The parsed frontmatter — only name/description are read; other
/// agentskills.io fields are ignored (serde's default).
#[derive(Debug, Default, PartialEq, Eq, serde::Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// Parse the `---`-delimited YAML frontmatter (serde_yaml — a proper
/// dependency, never string-splitting; BOM and CRLF tolerated).
/// `Ok(None)` when the file has no frontmatter at all; `Err` when it
/// is broken (the caller skips + warns).
fn parse_frontmatter(content: &str) -> Result<Option<Frontmatter>, String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.lines();
    if lines.next().is_none_or(|line| line.trim_end() != "---") {
        return Ok(None);
    }
    let mut yaml = String::new();
    for line in lines {
        if line.trim_end() == "---" {
            return serde_yaml::from_str(&yaml)
                .map(Some)
                .map_err(|err| err.to_string());
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    Err("frontmatter is never closed by a second `---` line".to_string())
}

/// The field rules: a missing/empty `description` drops the skill (a
/// skill without a trigger description is undiscoverable noise); a
/// missing `name` falls back to the parent directory name (pi's
/// rules).
fn resolve_fields(fm: Frontmatter, parent_dir_name: &str) -> Option<(String, String)> {
    let description = fm.description.filter(|d| !d.trim().is_empty())?;
    let name = fm
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| parent_dir_name.to_string());
    Some((name, description))
}

/// Parse one SKILL.md into a [`SkillEntry`]; malformed input
/// (unreadable file, broken YAML, missing description) skips with a
/// warn. The base dir is canonicalized at discovery so the tool's
/// confinement compares canonical paths.
fn load_skill(skill_file: &Path, level: SkillLevel) -> Option<SkillEntry> {
    let content = match std::fs::read_to_string(skill_file) {
        Ok(content) => content,
        Err(err) => {
            tracing::warn!(path = %skill_file.display(), error = %err, "skipping unreadable SKILL.md");
            return None;
        }
    };
    let frontmatter = match parse_frontmatter(&content) {
        Ok(Some(frontmatter)) => frontmatter,
        Ok(None) => Frontmatter::default(),
        Err(err) => {
            tracing::warn!(path = %skill_file.display(), error = %err, "skipping SKILL.md with broken frontmatter");
            return None;
        }
    };
    // Sanctioned crash (AGENTS.md doctrine): a scanned SKILL.md path
    // is `<dir>/SKILL.md` — it always has a parent directory.
    #[allow(clippy::expect_used)]
    let skill_dir = skill_file
        .parent()
        .expect("a SKILL.md path always has a parent");
    let parent_dir_name = skill_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some((name, description)) = resolve_fields(frontmatter, &parent_dir_name) else {
        tracing::warn!(path = %skill_file.display(), "skipping skill without a description");
        return None;
    };
    let base_dir = std::fs::canonicalize(skill_dir).unwrap_or_else(|err| {
        // The dir was just scanned; a canonicalize failure here is
        // exotic — keep the unresolved path as the base (only a
        // symlinked dir stays unresolved), not silent.
        tracing::warn!(path = %skill_dir.display(), error = %err, "could not canonicalize the skill dir; using the unresolved path");
        skill_dir.to_path_buf()
    });
    Some(SkillEntry {
        name,
        description,
        skill_file: base_dir.join("SKILL.md"),
        base_dir,
        level,
    })
}

/// XML-escape a catalog value (the same five characters every
/// reference escapes).
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

impl Skills {
    /// The entry named `name`, when discovered.
    pub fn lookup(&self, name: &str) -> Option<&SkillEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Whether discovery found nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The comma-joined names for error messages ("none (no skills
    /// were discovered)" when empty).
    fn names(&self) -> String {
        if self.entries.is_empty() {
            return "none (no skills were discovered)".to_string();
        }
        self.entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The wire snapshot for the `skills_available` announcement.
    pub fn available(&self) -> Vec<tabit_protocol::AvailableSkill> {
        self.entries
            .iter()
            .map(|e| tabit_protocol::AvailableSkill {
                name: e.name.clone(),
                description: e.description.clone(),
                location: e.skill_file.display().to_string(),
                level: e.level.as_str().to_string(),
            })
            .collect()
    }

    /// The prompt catalog: one block listing name, description, and
    /// location per skill. EMPTY when discovery found nothing — the
    /// prompt never carries an empty block.
    pub fn render_catalog(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "The following skills provide specialized instructions for specific tasks. \
             Invoke one with the `skill` tool (`name` selects the skill; `rel_path` \
             defaults to SKILL.md and may name a file to read or a directory to list \
             within the skill). Reads are confined to the skill's directory.\n\n\
             <available_skills>\n",
        );
        for entry in &self.entries {
            out.push_str("  <skill>\n");
            out.push_str(&format!("    <name>{}</name>\n", xml_escape(&entry.name)));
            out.push_str(&format!(
                "    <description>{}</description>\n",
                xml_escape(&entry.description)
            ));
            out.push_str(&format!(
                "    <location>{}</location>\n",
                xml_escape(&entry.skill_file.display().to_string())
            ));
            out.push_str("  </skill>\n");
        }
        out.push_str("</available_skills>");
        out
    }
}

/// The `skill` tool's mount point — the assembly's contextual
/// [`dynamic_contextual`] shape (the subagent tool's sibling).
pub fn skill_tool() -> DynamicTool {
    rig_agent::tool::dynamic_contextual(Skill)
}

/// Invoke a skill from the available-skills catalog — the model
/// expresses intent by name; that skills are host files stays behind
/// the tool.
#[rig_tool(
    description = "Invoke a skill from the available-skills catalog. `name` selects the \
                   skill; `rel_path` (default SKILL.md) selects a file to read or a \
                   directory to list within the skill's directory. Reads are confined \
                   to the skill's base directory."
)]
pub async fn skill(
    #[rig(context)] context: &mut ToolContext,
    name: String,
    rel_path: Option<String>,
) -> Result<ToolOutput, ToolExecutionError> {
    let skills = context.get::<Arc<Skills>>().cloned().ok_or_else(|| {
        ToolExecutionError::other(
            "skills are not available in this session — the assembly did not discover them",
        )
    })?;
    let entry = skills.lookup(&name).ok_or_else(|| {
        ToolExecutionError::other(format!(
            "no skill named `{name}` — available: {}",
            skills.names()
        ))
    })?;
    let rel = rel_path
        .as_deref()
        .filter(|r| !r.is_empty())
        .unwrap_or("SKILL.md");
    let target = resolve_confined(entry, rel)?;
    let meta = std::fs::metadata(&target).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ToolExecutionError::other(format!("path not found in skill `{}`: {rel}", entry.name))
        } else {
            ToolExecutionError::other(format!(
                "could not read {rel} in skill `{}`: {err}",
                entry.name
            ))
        }
    })?;
    if meta.is_dir() {
        let read = std::fs::read_dir(&target).map_err(|err| {
            ToolExecutionError::other(format!(
                "could not list {rel} in skill `{}`: {err}",
                entry.name
            ))
        })?;
        let mut lines: Vec<String> = Vec::new();
        for entry_dir in read {
            let Ok(entry_dir) = entry_dir else {
                tracing::warn!(dir = %target.display(), "skipping an unreadable skill dir entry");
                continue;
            };
            let mut line = entry_dir.file_name().to_string_lossy().into_owned();
            if entry_dir.path().is_dir() {
                line.push('/');
            }
            lines.push(line);
        }
        lines.sort();
        return Ok(ToolOutput::text(format!(
            "{}{}",
            lines.join("\n"),
            footer(entry)
        )));
    }
    let content = std::fs::read_to_string(&target).map_err(|err| {
        ToolExecutionError::other(format!(
            "could not read {rel} in skill `{}`: {err}",
            entry.name
        ))
    })?;
    Ok(ToolOutput::text(format!("{}{}", content, footer(entry))))
}

/// The base-directory footer appended to success results — advanced
/// operations (grep/find through the skill) stay possible under the
/// normal tools.
fn footer(entry: &SkillEntry) -> String {
    format!("\n\n[Skill base directory: {}]", entry.base_dir.display())
}

/// Resolve `rel` inside the skill's base dir, confined: the lexical
/// check first (absolute paths and `..` components never resolve),
/// then canonicalize at read time so symlinks cannot escape. An
/// escaping path is a model-visible error, never a read.
fn resolve_confined(entry: &SkillEntry, rel: &str) -> Result<PathBuf, ToolExecutionError> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ToolExecutionError::other(format!(
            "rel_path `{rel}` escapes the skill directory — reads are confined to {}",
            entry.base_dir.display()
        )));
    }
    let resolved = entry.base_dir.join(rel_path);
    // Canonicalize the TARGET before reading: a nonexistent target is
    // the not-found error (nothing read); a symlink resolving outside
    // the base is an escape (the base was canonical at discovery, and
    // the caller re-canonicalizes it to keep the comparison honest if
    // it moved).
    let canonical = std::fs::canonicalize(&resolved).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ToolExecutionError::other(format!("path not found in skill `{}`: {rel}", entry.name))
        } else {
            ToolExecutionError::other(format!(
                "could not read {rel} in skill `{}`: {err}",
                entry.name
            ))
        }
    })?;
    let canonical_base = std::fs::canonicalize(&entry.base_dir).map_err(|err| {
        ToolExecutionError::other(format!(
            "skill `{}` is no longer readable at {}: {err}",
            entry.name,
            entry.base_dir.display()
        ))
    })?;
    if !canonical.starts_with(&canonical_base) {
        return Err(ToolExecutionError::other(format!(
            "rel_path `{rel}` escapes the skill directory through a symlink — reads are \
             confined to {}",
            entry.base_dir.display()
        )));
    }
    Ok(canonical)
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
