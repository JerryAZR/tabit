//! Skills tests: the frontmatter rules, the four-source ladder, the
//! scan shape, the catalog render, and the confined tool.

use super::*;
use rig_agent::tool::ToolContext;
use std::fs;
use std::path::{Path, PathBuf};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("tabit-session-tests")
        .join(format!("skills-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, content).expect("write file");
}

fn skill_dir(root: &Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
    let dir = root.join(name);
    write(
        &dir.join("SKILL.md"),
        &format!("---\n{frontmatter}---\n{body}"),
    );
    dir
}

// ---- frontmatter parsing

#[test]
fn frontmatter_reads_name_and_description() {
    let parsed = parse_frontmatter("---\nname: lint\ndescription: Lint the workspace.\n---\nbody")
        .expect("parses")
        .expect("present");
    assert_eq!(parsed.name.as_deref(), Some("lint"));
    assert_eq!(parsed.description.as_deref(), Some("Lint the workspace."));
}

#[test]
fn frontmatter_tolerates_a_bom_and_crlf() {
    let parsed = parse_frontmatter(
        "---\r\nname: lint\r\ndescription: Lint.\r\n---\r\nbody",
    )
    .expect("parses")
    .expect("present");
    assert_eq!(parsed.name.as_deref(), Some("lint"));
}

#[test]
fn frontmatter_ignores_unknown_fields_and_absence_is_none() {
    let parsed = parse_frontmatter("---\nname: lint\nwhen: always\n---\n")
        .expect("parses")
        .expect("present");
    assert_eq!(parsed.description, None, "unknown fields ignored");
    assert_eq!(parse_frontmatter("# no frontmatter\nbody").expect("parses"), None);
}

#[test]
fn broken_frontmatter_is_an_error() {
    assert!(parse_frontmatter("---\nname: [unclosed\n---\n").is_err());
    assert!(parse_frontmatter("---\nname: never-closed\n").is_err());
}

// ---- field rules (pi's)

#[test]
fn a_missing_description_drops_and_a_missing_name_falls_back() {
    assert!(resolve_fields(Frontmatter::default(), "dir").is_none());
    let (name, description) = resolve_fields(
        Frontmatter {
            name: None,
            description: Some("Does the thing.".to_string()),
        },
        "my-skill",
    )
    .expect("resolves");
    assert_eq!(name, "my-skill");
    assert_eq!(description, "Does the thing.");
}

// ---- discovery: the four-source ladder

#[test]
fn the_ladder_merges_with_the_ruled_precedence() {
    let home = temp_dir("ladder-home");
    let cwd = temp_dir("ladder-cwd");
    // The same name in all four sources: the last scanned wins.
    skill_dir(&home.join(".agents/skills"), "shared", "description: agents-home\n", "v1");
    skill_dir(&home.join(".tabit/skills"), "shared", "description: tabit-home\n", "v2");
    skill_dir(&cwd.join(".agents/skills"), "shared", "description: agents-cwd\n", "v3");
    skill_dir(&cwd.join(".tabit/skills"), "shared", "description: tabit-cwd\n", "v4");
    // Different names merge, not replace.
    skill_dir(&home.join(".agents/skills"), "home-only", "description: kept\n", "x");
    let skills = discover_with_home(Some(&home), &cwd);
    assert_eq!(skills.lookup("shared").expect("found").description, "tabit-cwd");
    assert_eq!(
        skills.lookup("shared").expect("found").level,
        SkillLevel::Workspace
    );
    assert!(skills.lookup("home-only").is_some(), "merge, not replace");
    let wire = skills.available();
    assert_eq!(wire.len(), 2);
    let shared = wire.iter().find(|s| s.name == "shared").expect("shared");
    assert_eq!(shared.level, "workspace");
    let home_only = wire.iter().find(|s| s.name == "home-only").expect("home-only");
    assert_eq!(home_only.level, "user");
}

#[test]
fn a_missing_home_scans_the_cwd_sources_only() {
    let cwd = temp_dir("no-home");
    skill_dir(&cwd.join(".agents/skills"), "only", "description: one\n", "body");
    let skills = discover_with_home(None, &cwd);
    assert_eq!(skills.lookup("only").expect("found").level, SkillLevel::Workspace);
}

// ---- discovery: the scan shape

#[test]
fn skill_dirs_are_leaves_dotdirs_and_loose_files_are_not_skills() {
    let cwd = temp_dir("scan");
    let root = cwd.join(".tabit/skills");
    // A plain skill.
    skill_dir(&root, "plain", "description: plain\n", "body");
    // A skill nested under a non-skill directory — found (recursive).
    skill_dir(&root.join("group/nested"), "deep", "description: deep\n", "body");
    // A directory containing SKILL.md does not recurse deeper: the
    // skill inside the skill is NOT discovered.
    write(&root.join("outer/SKILL.md"), "---\ndescription: outer\n---\nbody");
    write(&root.join("outer/inner/SKILL.md"), "---\ndescription: inner\n---\nbody");
    // A dotdir skill is skipped.
    skill_dir(&root.join(".hidden"), "secret", "description: nope\n", "body");
    // A loose .md file is never a skill.
    write(&root.join("loose.md"), "---\ndescription: nope\n---\nbody");
    let skills = discover_with_home(None, &cwd);
    let wire = skills.available();
    let names: Vec<&str> = wire.iter().map(|s| s.name.as_str()).collect();
    // Sorted scan order: the group's nested skill first, then the
    // outer leaf, then plain.
    assert_eq!(names, ["deep", "outer", "plain"], "{names:?}");
}

#[test]
fn malformed_skills_skip_without_failing_discovery() {
    let cwd = temp_dir("malformed");
    let root = cwd.join(".agents/skills");
    skill_dir(&root, "good", "description: good\n", "body");
    // Broken YAML.
    write(&root.join("broken/SKILL.md"), "---\nname: [unclosed\n---\nbody");
    // No description — dropped.
    write(&root.join("silent/SKILL.md"), "---\nname: silent\n---\nbody");
    let skills = discover_with_home(None, &cwd);
    assert!(skills.lookup("good").is_some());
    assert!(skills.lookup("broken").is_none());
    assert!(skills.lookup("silent").is_none());
}

// ---- the catalog render

#[test]
fn the_catalog_escapes_and_empty_renders_nothing() {
    let cwd = temp_dir("render");
    skill_dir(&cwd.join(".tabit/skills"), "xss", "description: <injection> & such\n", "body");
    let skills = discover_with_home(None, &cwd);
    let catalog = skills.render_catalog();
    assert!(catalog.contains("<name>xss</name>"), "{catalog}");
    assert!(catalog.contains("&lt;injection&gt;"), "{catalog}");
    assert!(catalog.contains("<available_skills>"), "{catalog}");
    assert_eq!(Skills::default().render_catalog(), "");
}

// ---- the tool

/// A context carrying the catalog, the session-cwd convention of the
/// contextual tools' tests.
fn context_for(skills: &Skills) -> ToolContext {
    let mut context = ToolContext::new();
    context.insert(std::sync::Arc::new(skills.clone()));
    context
}

#[tokio::test]
async fn the_tool_returns_the_body_with_the_base_dir_footer() {
    let cwd = temp_dir("tool-body");
    skill_dir(&cwd.join(".tabit/skills"), "lint", "description: lint\n", "# the skill body");
    let skills = discover_with_home(None, &cwd);
    let mut context = context_for(&skills);
    let output = skill(&mut context, "lint".to_string(), None)
        .await
        .expect("runs");
    let text = output.render().to_string();
    assert!(text.contains("# the skill body"), "{text}");
    assert!(text.contains("[Skill base directory: "), "{text}");
}

#[tokio::test]
async fn the_tool_lists_a_directory_rel_path() {
    let cwd = temp_dir("tool-list");
    let dir = skill_dir(&cwd.join(".tabit/skills"), "pack", "description: pack\n", "body");
    write(&dir.join("scripts/run.py"), "print('hi')");
    write(&dir.join("references/a.md"), "a");
    let skills = discover_with_home(None, &cwd);
    let mut context = context_for(&skills);
    let output = skill(&mut context, "pack".to_string(), Some("scripts".to_string()))
        .await
        .expect("runs");
    let text = output.render().to_string();
    assert!(text.contains("run.py"), "{text}");
    assert!(!text.contains("a.md"), "direct entries only: {text}");
}

#[tokio::test]
async fn escapes_are_refused_with_nothing_read() {
    let cwd = temp_dir("tool-escape");
    skill_dir(&cwd.join(".tabit/skills"), "safe", "description: safe\n", "body");
    write(&cwd.join("outside.txt"), "secret");
    let skills = discover_with_home(None, &cwd);
    let mut context = context_for(&skills);
    for bad in ["../outside.txt", "a/../../outside.txt", "C:/Windows/system32"] {
        let error = skill(&mut context, "safe".to_string(), Some(bad.to_string()))
            .await
            .expect_err("refused");
        assert!(error.to_string().contains("escapes"), "{bad}: {error}");
    }
}

#[tokio::test]
async fn a_symlink_escape_is_refused_at_read_time() {
    let cwd = temp_dir("tool-symlink");
    let dir = skill_dir(&cwd.join(".tabit/skills"), "linked", "description: l\n", "body");
    write(&cwd.join("outside.txt"), "secret");
    // Windows without developer mode cannot create symlinks; a
    // junction needs no privilege and serves the same escape.
    let link = dir.join("leak");
    #[cfg(windows)]
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&cwd)
        .output()
        .expect("mklink spawn")
        .status
        .success();
    #[cfg(not(windows))]
    let made = std::os::unix::fs::symlink(&cwd, &link).is_ok();
    if !made {
        return; // the environment cannot stage the link; the lexical arms are covered above
    }
    let skills = discover_with_home(None, &cwd);
    let mut context = context_for(&skills);
    let error = skill(&mut context, "linked".to_string(), Some("leak/outside.txt".to_string()))
        .await
        .expect_err("the symlink escape is refused");
    assert!(error.to_string().contains("symlink"), "{error}");
}

#[tokio::test]
async fn an_unknown_name_lists_what_exists() {
    let cwd = temp_dir("tool-unknown");
    skill_dir(&cwd.join(".tabit/skills"), "known", "description: k\n", "body");
    let skills = discover_with_home(None, &cwd);
    let mut context = context_for(&skills);
    let error = skill(&mut context, "nope".to_string(), None)
        .await
        .expect_err("unknown name");
    let message = error.to_string();
    assert!(message.contains("no skill named `nope`"), "{message}");
    assert!(message.contains("known"), "{message}");
}

#[tokio::test]
async fn without_a_mounted_catalog_the_tool_refuses() {
    let mut context = ToolContext::new();
    let error = skill(&mut context, "any".to_string(), None)
        .await
        .expect_err("no catalog mounted");
    assert!(error.to_string().contains("did not discover"), "{error}");
}
