use super::*;
use std::fs;
use std::path::PathBuf;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("tabit-session-tests")
        .join(format!("prompt-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, content).expect("write instruction file");
}

fn context_file(path: &Path, content: &str) -> ContextFile {
    ContextFile {
        path: path.to_path_buf(),
        content: content.to_string(),
    }
}

#[test]
fn compose_without_files_is_base_and_env_only() {
    let prompt = compose_system_prompt(Path::new("C:\\work\\proj"), "2026-08-15", &[]);
    assert!(prompt.starts_with("You are tabit"));
    assert!(prompt.contains("cwd: C:/work/proj\n"));
    assert!(prompt.contains(&format!("platform: {}\n", std::env::consts::OS)));
    assert!(prompt.contains("date: 2026-08-15 (UTC)\n"));
    assert!(!prompt.contains("<project_context>"));
}

#[test]
fn compose_puts_env_first_then_home_then_cwd() {
    let home = context_file(Path::new("C:/u/.tabit/AGENTS.md"), "home rules");
    let cwd = context_file(Path::new("C:\\work\\proj\\AGENTS.md"), "project rules");
    let prompt = compose_system_prompt(Path::new("C:/work/proj"), "2026-08-15", &[home, cwd]);

    let env = prompt.find("<environment_context>").expect("env block");
    let home_at = prompt.find("home rules").expect("home content");
    let cwd_at = prompt.find("project rules").expect("cwd content");
    assert!(env < home_at && home_at < cwd_at);

    // Paths are slash-normalized; content is verbatim.
    assert!(prompt.contains("<file path=\"C:/u/.tabit/AGENTS.md\">"));
    assert!(prompt.contains("<file path=\"C:/work/proj/AGENTS.md\">"));
    assert!(prompt.contains(">\nhome rules\n</file>"));
    assert!(prompt.contains(">\nproject rules\n</file>"));
}

#[test]
fn discover_finds_nothing_when_absent() {
    let home = temp_dir("absent-home");
    let cwd = temp_dir("absent-cwd");
    assert!(discover_context_files(&home, &cwd).is_ok_and(|f| f.is_empty()));
}

#[test]
fn discover_prefers_tabit_over_agents_fallback() {
    let home = temp_dir("both-home");
    let cwd = temp_dir("both-cwd");
    write(&home.join(".tabit").join("AGENTS.md"), "tabit-level");
    write(&home.join(".agents").join("AGENTS.md"), "agents-level");

    let files = discover_context_files(&home, &cwd).expect("discover");
    assert_eq!(
        files,
        [context_file(
            &home.join(".tabit").join("AGENTS.md"),
            "tabit-level"
        )]
    );
}

#[test]
fn discover_falls_back_to_agents_dir() {
    let home = temp_dir("fallback-home");
    let cwd = temp_dir("fallback-cwd");
    let agents_path = home.join(".agents").join("AGENTS.md");
    write(&agents_path, "agents-level");

    let files = discover_context_files(&home, &cwd).expect("discover");
    assert_eq!(files, [context_file(&agents_path, "agents-level")]);
}

#[test]
fn discover_appends_cwd_file_after_home_file() {
    let home = temp_dir("pair-home");
    let cwd = temp_dir("pair-cwd");
    let home_path = home.join(".tabit").join("AGENTS.md");
    let cwd_path = cwd.join("AGENTS.md");
    write(&home_path, "home-level");
    write(&cwd_path, "project-level");

    let files = discover_context_files(&home, &cwd).expect("discover");
    assert_eq!(
        files,
        [
            context_file(&home_path, "home-level"),
            context_file(&cwd_path, "project-level"),
        ]
    );
}

#[test]
fn discover_reports_unreadable_file_loudly() {
    let home = temp_dir("unreadable-home");
    let cwd = temp_dir("unreadable-cwd");
    // A directory where the instruction file should be: present on disk,
    // but not a readable UTF-8 file.
    fs::create_dir_all(home.join(".tabit").join("AGENTS.md")).expect("create blocker dir");

    match discover_context_files(&home, &cwd) {
        Err(SessionError::Io { path, .. }) => {
            assert_eq!(path, home.join(".tabit").join("AGENTS.md"))
        }
        other => panic!("expected an Io error, got {other:?}"),
    }
}

#[test]
fn discover_reports_unreadable_cwd_file_loudly() {
    let home = temp_dir("unreadable-home-2");
    let cwd = temp_dir("unreadable-cwd-2");
    // No home file, so discovery reaches the cwd candidate.
    fs::create_dir_all(cwd.join("AGENTS.md")).expect("create blocker dir");

    match discover_context_files(&home, &cwd) {
        Err(SessionError::Io { path, .. }) => assert_eq!(path, cwd.join("AGENTS.md")),
        other => panic!("expected an Io error, got {other:?}"),
    }
}

#[test]
fn utc_date_is_bare_ymd() {
    let date = utc_date();
    let bytes = date.as_bytes();
    assert_eq!(date.len(), 10, "YYYY-MM-DD is ten characters");
    assert!(bytes.iter().enumerate().all(|(i, b)| {
        let separator = i == 4 || i == 7;
        (separator && *b == b'-') || (!separator && b.is_ascii_digit())
    }));
}

#[test]
fn build_with_home_requires_a_home() {
    let cwd = temp_dir("no-home-cwd");
    match build_with_home(None, &cwd) {
        Err(SessionError::Config { .. }) => {}
        other => panic!("expected a Config error, got {other:?}"),
    }
}

#[test]
fn build_with_home_composes_discovered_files() {
    let home = temp_dir("build-home");
    let cwd = temp_dir("build-cwd");
    let agents_path = home.join(".agents").join("AGENTS.md");
    let cwd_path = cwd.join("AGENTS.md");
    write(&agents_path, "agents-level");
    write(&cwd_path, "project-level");

    let prompt = build_with_home(Some(home.clone()), &cwd).expect("build");
    assert!(prompt.starts_with("You are tabit"));
    assert!(prompt.contains(&format!("cwd: {}\n", normalize(&cwd))));
    assert!(prompt.contains("date: "));
    let agents_at = prompt.find("agents-level").expect("agents content");
    let cwd_at = prompt.find("project-level").expect("cwd content");
    assert!(agents_at < cwd_at);
}

#[test]
fn build_system_prompt_reads_the_real_home() {
    // The public wrapper differs from `build_with_home` only in resolving
    // the real home directory; machine-independent assertions only.
    let cwd = temp_dir("wrapper-cwd");
    let prompt = build_system_prompt(&cwd).expect("build");
    assert!(prompt.starts_with("You are tabit"));
    assert!(prompt.contains(&format!("cwd: {}\n", normalize(&cwd))));
}
