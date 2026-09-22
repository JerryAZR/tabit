//! Path preprocessing utilities — a faithful port of pi-sanity's
//! `path-utils.ts`. Expands tildes, `{{VAR}}` placeholders and `$ENV`
//! variables, and normalizes paths. No checking logic — pure
//! preprocessing only.
//!
//! TS builds on Node's `path.posix` / `path.win32` (`resolve`,
//! `normalize`, `isAbsolute`). Rust's `std::path` follows a different
//! (Windows-RFC semantics) algorithm, so the exact Node behavior the
//! port depends on is reproduced below in [`node_path`] — the subset
//! actually reachable from [`preprocess_path`]: absolute/relative
//! resolution, `.`/`..` collapsing, separator normalization, drive
//! roots, and UNC prefixes. Hand-rolled because no maintained crate
//! implements Node's algorithm (`path-clean` is POSIX-only and drops
//! trailing slashes); it is unit-tested against the Node-documented
//! cases the gate relies on.

/// Target platform for win32-specific handling (TS: `process.platform`
/// / Node's `NodeJS.Platform`, of which only `win32` is ever
/// distinguished).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// Windows semantics: drive letters, case-insensitive matching,
    /// git-bash input conversion.
    Win32,
    /// Everything else (POSIX semantics).
    Other,
}

impl Platform {
    /// The platform this build runs on (TS: `process.platform`).
    pub fn native() -> Self {
        if cfg!(windows) {
            Platform::Win32
        } else {
            Platform::Other
        }
    }

    pub fn is_win32(self) -> bool {
        self == Platform::Win32
    }
}

/// The context a path is resolved in (TS `PathContext`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathContext {
    pub cwd: String,
    pub home: String,
    pub repo: Option<String>,
    pub tmpdir: String,
    pub platform: Platform,
}

/// Options for [`preprocess_path`] (TS `PreprocessOptions`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PreprocessOptions {
    /// Expand `~` to the home directory.
    pub expand_tilde: bool,
    /// Expand `{{VAR}}` syntax (config-time only).
    pub expand_braces: bool,
    /// Expand `$ENV_VAR` syntax.
    pub expand_env_vars: bool,
    /// Resolve relative paths to absolute.
    pub resolve_relative: bool,
    /// Normalize separators (TS: `"forward"` | `"native"`; every
    /// caller in the port uses forward slashes, like both TS default
    /// sets).
    pub separator_forward: bool,
    /// Canonicalize drive letters to `/x/` form for matching
    /// (TS `canonicalize`, default true; cwd tracking passes false).
    pub canonicalize: bool,
}

/// Default options for config pattern preprocessing (TS
/// `CONFIG_DEFAULTS`).
pub(crate) const CONFIG_DEFAULTS: PreprocessOptions = PreprocessOptions {
    expand_tilde: true,
    expand_braces: true,
    expand_env_vars: true,
    resolve_relative: true,
    separator_forward: true,
    canonicalize: true,
};

/// Default options for runtime path preprocessing (TS
/// `RUNTIME_DEFAULTS`).
pub(crate) const RUNTIME_DEFAULTS: PreprocessOptions = PreprocessOptions {
    expand_tilde: true,
    expand_braces: false,
    expand_env_vars: true,
    resolve_relative: true,
    separator_forward: true,
    canonicalize: true,
};

/// Expand `~` or `~/...` / `~\...` to the home directory.
/// `~user` and `abc~def` are left alone (TS `TILDE_REGEX`:
/// `^~(?=$|[/\\])`).
pub(crate) fn expand_tilde(input: &str, home_dir: &str) -> String {
    if input == "~" {
        return home_dir.to_string();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return format!("{home_dir}/{rest}");
    }
    if let Some(rest) = input.strip_prefix("~\\") {
        return format!("{home_dir}\\{rest}");
    }
    input.to_string()
}

/// Expand `{{HOME}}`, `{{CWD}}`, `{{REPO}}`, `{{TMPDIR}}` (TS
/// `expandBraces`).
pub(crate) fn expand_braces(input: &str, context: &PathContext) -> String {
    input
        .replace("{{HOME}}", &context.home)
        .replace("{{CWD}}", &context.cwd)
        .replace("{{REPO}}", context.repo.as_deref().unwrap_or(&context.cwd))
        .replace("{{TMPDIR}}", &context.tmpdir)
}

/// Expand `$ENV_VAR` syntax; unset variables stay literal (TS
/// `expandEnvVars`, `\$([A-Za-z_][A-Za-z0-9_]*)` global replace).
pub(crate) fn expand_env_vars(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            // Name: [A-Za-z_][A-Za-z0-9_]*
            let mut j = i + 1;
            if j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
                j += 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let name = &input[i + 1..j];
                match std::env::var(name) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => out.push_str(&input[i..j]),
                }
                i = j;
                continue;
            }
        }
        // Copy the whole next char (UTF-8 safe).
        let ch = input[i..].chars().next().unwrap_or('$');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Replace all backslashes with forward slashes (TS
/// `normalizeSeparators`; picomatch treats backslashes as escapes).
pub(crate) fn normalize_separators(input: &str) -> String {
    input.replace('\\', "/")
}

/// Native win32 drive prefix: `"C:"` at the start of a path (TS
/// `DRIVE_PREFIX_RE`).
pub(crate) fn has_drive_prefix(input: &str) -> bool {
    let b = input.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Native absolute win32 drive path: `"C:/..."` or `"C:\..."` (TS
/// `DRIVE_ABS_RE`).
pub(crate) fn is_drive_absolute(input: &str) -> bool {
    let b = input.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

/// Bare win32 drive root after separator normalization: `"C:/"` (TS
/// `DRIVE_ROOT_RE`).
fn is_drive_root(input: &str) -> bool {
    let b = input.as_bytes();
    b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/'
}

/// Git-bash drive path: `/c/...` or `/c` (single letter + slash or
/// end; deliberately does not match `/tmp/...`). TS
/// `GIT_BASH_DRIVE_RE`.
fn git_bash_drive_prefix_len(input: &str) -> Option<usize> {
    let b = input.as_bytes();
    if b.len() >= 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() {
        if b.len() == 2 {
            return Some(2);
        }
        if b[2] == b'/' {
            return Some(3);
        }
    }
    None
}

/// Convert a git-bash drive path to native win32 form:
/// `/c/Users` -> `C:/Users` (TS `convertGitBashDrivePath`; pure string
/// transform, callers gate on win32).
pub(crate) fn convert_git_bash_drive_path(input: &str) -> String {
    match git_bash_drive_prefix_len(input) {
        Some(len) => {
            let drive = input[1..2].to_ascii_uppercase();
            format!("{drive}:/{}", &input[len..])
        }
        None => input.to_string(),
    }
}

/// Canonicalize a native win32 drive prefix to `/x/` form for matching:
/// `C:/path` -> `/c/path`; `C:` -> `/c`. Idempotent; leaves `/x/...`,
/// POSIX, and UNC paths untouched; distinct drives stay distinct (TS
/// `canonicalizeDrive`).
pub(crate) fn canonicalize_drive(input: &str) -> String {
    if has_drive_prefix(input) {
        let drive = input[0..1].to_ascii_lowercase();
        format!("/{drive}{}", &input[2..])
    } else {
        input.to_string()
    }
}

/// Unified path preprocessing (TS `preprocessPath`). Steps in TS
/// order; the win32 step 0 input conversion, form-appropriate
/// resolve/normalize semantics, and drive-root trailing-slash
/// preservation are all preserved exactly.
pub(crate) fn preprocess_path(
    input: &str,
    context: &PathContext,
    options: PreprocessOptions,
) -> String {
    let win32 = context.platform.is_win32();
    let mut result = input.to_string();

    // Step 0: Convert git-bash drive input (/c/...) to native form
    // (win32 only). This is the input boundary: /c/... and C:\...
    // denote the same path.
    if win32 {
        result = convert_git_bash_drive_path(&result);
    }

    // Step 1: Expand tilde.
    if options.expand_tilde {
        result = expand_tilde(&result, &context.home);
    }

    // Step 2: Expand {{VAR}} syntax.
    if options.expand_braces {
        result = expand_braces(&result, context);
    }

    // Step 3: Expand $ENV_VAR syntax.
    if options.expand_env_vars {
        result = expand_env_vars(&result);
    }

    // Steps 4-5: Resolve and normalize with form-appropriate
    // semantics. Drive-letter and UNC paths use win32 rules.
    // Root-relative POSIX paths (/etc, /tmp) stay drive-less: on win32
    // they are MSYS/Git-Bash mounts, NOT C:\-rooted, so
    // drive-anchoring them would be wrong. Relative paths resolve
    // against the cwd using the cwd's own semantics.
    let is_posix_form = |p: &str| p.starts_with('/') && !p.starts_with("//");
    let posix_form =
        is_posix_form(&result) || (!has_drive_prefix(&result) && is_posix_form(&context.cwd));
    let absolute = if posix_form {
        result.starts_with('/')
    } else {
        node_path::win32_is_absolute(&result)
    };
    if options.resolve_relative && !absolute {
        result = if posix_form || !win32 {
            node_path::posix_resolve(&context.cwd, &result)
        } else {
            node_path::win32_resolve(&context.cwd, &result)
        };
    }
    result = if posix_form || !win32 {
        node_path::posix_normalize(&result)
    } else {
        node_path::win32_normalize(&result)
    };

    // Step 6: Normalize separators (forward slashes for
    // cross-platform matching).
    if options.separator_forward {
        result = normalize_separators(&result);
    }

    // Step 7: Canonicalize win32 drive letters for matching. C:/path
    // becomes /c/path; a bare drive root (C: or C:/) becomes /c.
    // Distinct drives stay distinct (/c/file != /d/file).
    if options.canonicalize {
        result = canonicalize_drive(&result);
    }

    // Step 8: Strip trailing slashes (except for root "/" and win32
    // drive roots like "C:/", which must keep their slash: a bare "C:"
    // is a drive-RELATIVE path and would mis-resolve against the
    // process cwd).
    if result.len() > 1 && result.ends_with('/') && !is_drive_root(&result) {
        result.truncate(result.len() - 1);
    }

    result
}

/// Preprocess a config pattern for glob matching (TS
/// `preprocessConfigPattern`). Patterns starting with `/**` match
/// anywhere (absolute glob) and skip relative resolution.
///
/// Public because the invariant needs it: patterns in a
/// [`crate::config::SanityConfig`] are ALWAYS already preprocessed —
/// the loader does it (one site), and any hand-built config must apply
/// this same transformation before storing patterns. Checking never
/// preprocesses.
pub fn preprocess_config_pattern(pattern: &str, context: &PathContext) -> String {
    let mut options = CONFIG_DEFAULTS;
    if pattern.starts_with("/**") {
        options.resolve_relative = false;
    }
    preprocess_path(pattern, context, options)
}

/// Preprocess a runtime file path for checking (TS
/// `preprocessRuntimePath`): expands tilde and env vars, resolves to
/// absolute, normalizes to forward slashes.
///
/// Public for the same reason as [`preprocess_config_pattern`]: the
/// TS unit tests called it directly, and the ported corpus does too.
pub fn preprocess_runtime_path(file_path: &str, context: &PathContext) -> String {
    preprocess_path(file_path, context, RUNTIME_DEFAULTS)
}

/// Check if a string contains characters that make it impossible to be
/// a valid path.
///
/// DISABLED: always returns false to avoid rejecting valid paths. The
/// original check rejected valid glob patterns like `*.txt` and paths
/// with special characters like `file#1`, causing security bypasses
/// (TS `clearlyNotAPath`, kept as the documented blind spot it is).
pub(crate) fn clearly_not_a_path(_s: &str) -> bool {
    false
}

/// Node `path` subset used by the preprocessing pipeline. The
/// algorithms mirror Node's `path.posix` / `path.win32` (see module
/// doc); only what the gate consumes is implemented.
pub(crate) mod node_path {
    /// Node `path.posix.resolve(cwd, p)`: absolute `p` normalizes to
    /// itself; otherwise `p` joins onto `cwd` (which may be `/`).
    pub fn posix_resolve(cwd: &str, p: &str) -> String {
        if p.starts_with('/') {
            posix_normalize(p)
        } else if cwd.ends_with('/') {
            posix_normalize(&format!("{cwd}{p}"))
        } else {
            posix_normalize(&format!("{cwd}/{p}"))
        }
    }

    /// Node `path.posix.normalize`: collapses separators, resolves
    /// `.`/`..`, keeps one trailing slash, keeps the root.
    pub fn posix_normalize(path: &str) -> String {
        if path.is_empty() {
            return ".".to_string();
        }
        let is_absolute = path.starts_with('/');
        let trailing = path.len() > 1 && path.ends_with('/');
        let mut segments: Vec<&str> = vec![];
        for segment in path.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    if let Some(last) = segments.last() {
                        if *last != ".." {
                            segments.pop();
                        } else if !is_absolute {
                            segments.push("..");
                        }
                    } else if !is_absolute {
                        segments.push("..");
                    }
                }
                other => segments.push(other),
            }
        }
        let mut out = segments.join("/");
        if out.is_empty() {
            out = if is_absolute { "/" } else { "." }.to_string();
        } else if is_absolute {
            out.insert(0, '/');
        }
        if trailing && out != "/" && !out.ends_with('/') {
            out.push('/');
        }
        out
    }

    fn is_win32_sep(b: u8) -> bool {
        b == b'/' || b == b'\\'
    }

    /// Node `path.win32.isAbsolute`: true for `\\`, `\foo`, `C:\a`,
    /// `//server`; false for `C:foo`, `foo`, `C:`.
    pub fn win32_is_absolute(path: &str) -> bool {
        let b = path.as_bytes();
        if b.is_empty() {
            return false;
        }
        if is_win32_sep(b[0]) {
            return true;
        }
        b.len() >= 3 && b[1] == b':' && is_win32_sep(b[2])
    }

    /// Node `path.win32.resolve(cwd, p)` for the two-path form the
    /// gate needs. Classification per Node: UNC absolute, drive
    /// absolute, device-rooted (uses the cwd's device), drive
    /// relative (uses the cwd's directory when the drives match),
    /// plain relative (joins the cwd).
    pub fn win32_resolve(cwd: &str, p: &str) -> String {
        let cwd = cwd.replace('/', "\\");
        if p.is_empty() {
            return win32_normalize(&cwd);
        }
        let path = p.replace('/', "\\");
        let b = path.as_bytes();
        let cwd_device = || {
            if cwd.len() >= 2 && cwd.as_bytes()[1] == b':' {
                cwd[..2].to_string()
            } else {
                String::new()
            }
        };
        // UNC root: \\server\...
        if b.len() >= 2 && is_win32_sep(b[0]) && is_win32_sep(b[1]) {
            return win32_normalize(&path);
        }
        // Drive prefix: C:...
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            if b.len() > 2 && is_win32_sep(b[2]) {
                return win32_normalize(&path); // drive-absolute
            }
            // Drive-relative (C:foo): same drive uses the cwd's
            // directory; otherwise the drive root.
            if cwd_device().eq_ignore_ascii_case(&path[..2]) {
                if cwd.len() > 2 {
                    if cwd.ends_with('\\') {
                        return win32_normalize(&format!("{cwd}{}", &path[2..]));
                    }
                    return win32_normalize(&format!("{cwd}\\{}", &path[2..]));
                }
                return win32_normalize(&path);
            }
            return win32_normalize(&path);
        }
        // Device-rooted (/foo or \foo): anchored to the cwd's device.
        if is_win32_sep(b[0]) {
            let device = cwd_device();
            return win32_normalize(&format!("{device}{path}"));
        }
        win32_normalize(&format!("{cwd}\\{path}"))
    }

    /// Node `path.win32.normalize`: forward slashes become
    /// backslashes, `.`/`..` collapse, `C:\` and UNC roots keep their
    /// trailing separator, and one trailing separator is otherwise
    /// preserved.
    pub fn win32_normalize(path: &str) -> String {
        if path.is_empty() {
            return ".".to_string();
        }
        let path = path.replace('/', "\\");
        let b = path.as_bytes();
        let len = b.len();
        if len == 1 {
            return if is_win32_sep(b[0]) {
                "\\".to_string()
            } else {
                path
            };
        }
        let had_trailing = is_win32_sep(b[len - 1]);

        let mut device = String::new();
        let mut root_end = 0;
        let mut is_absolute = false;
        if is_win32_sep(b[0]) {
            is_absolute = true;
            if is_win32_sep(b[1]) {
                // UNC: \\server\share\...
                let mut j = 2;
                while j < len && !is_win32_sep(b[j]) {
                    j += 1;
                }
                if j < len && j != 2 {
                    device = path[..j].to_string();
                    root_end = j + 1;
                } else {
                    root_end = 1;
                }
            } else {
                root_end = 1;
            }
        } else if b[1] == b':' && b[0].is_ascii_alphabetic() {
            device = path[..2].to_string();
            root_end = 2;
            if len > 2 && is_win32_sep(b[2]) {
                is_absolute = true;
                root_end = 3;
            }
        }

        let tail = normalize_windows_tail(&path[root_end.min(len)..], !is_absolute);
        let mut result = if !device.is_empty() {
            if tail.is_empty() {
                format!("{device}\\")
            } else {
                format!("{device}\\{tail}")
            }
        } else if is_absolute {
            if tail.is_empty() {
                "\\".to_string()
            } else {
                format!("\\{tail}")
            }
        } else if tail.is_empty() {
            ".".to_string()
        } else {
            tail
        };
        // Preserve one trailing separator (Node keeps it; "." and
        // already-rooted results are left alone).
        if had_trailing && result != "." && !result.ends_with('\\') {
            result.push('\\');
        }
        result
    }

    /// Node's `normalizeString` over backslash-separated segments;
    /// `allow_above_root` keeps leading `..` for relative tails.
    fn normalize_windows_tail(tail: &str, allow_above_root: bool) -> String {
        let mut segments: Vec<&str> = vec![];
        for segment in tail.split('\\') {
            match segment {
                "" | "." => {}
                ".." => match segments.last() {
                    Some(&"..") => {
                        if allow_above_root {
                            segments.push("..");
                        }
                    }
                    Some(_) => {
                        segments.pop();
                    }
                    None => {
                        if allow_above_root {
                            segments.push("..");
                        }
                    }
                },
                other => segments.push(other),
            }
        }
        segments.join("\\")
    }

    /// Node `path.win32.join`: concatenates with backslashes and
    /// normalizes (only the arities the gate needs).
    pub fn win32_join(parts: &[&str]) -> String {
        let joined: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
        win32_normalize(&joined.join("\\"))
    }
}

#[cfg(test)]
mod tests {
    #![cfg_attr(
        test,
        allow(
            clippy::err_expect,
            clippy::expect_used,
            clippy::indexing_slicing,
            clippy::panic,
            clippy::panic_in_result_fn,
            clippy::unreachable,
            clippy::unwrap_used
        )
    )]

    use super::*;

    fn posix_ctx() -> PathContext {
        PathContext {
            cwd: "/project".into(),
            home: "/home/user".into(),
            repo: Some("/project".into()),
            tmpdir: "/tmp".into(),
            platform: Platform::Other,
        }
    }

    fn win_ctx() -> PathContext {
        PathContext {
            platform: Platform::Win32,
            ..posix_ctx()
        }
    }

    #[test]
    fn posix_preprocessing_matches_path_utils_ts() {
        let ctx = posix_ctx();
        assert_eq!(
            preprocess_config_pattern("/simple/path", &ctx),
            "/simple/path"
        );
        assert_eq!(
            preprocess_config_pattern("{{HOME}}/.ssh/**", &ctx),
            "/home/user/.ssh/**"
        );
        assert_eq!(
            preprocess_config_pattern("{{CWD}}/file.txt", &ctx),
            "/project/file.txt"
        );
        assert_eq!(
            preprocess_config_pattern("{{REPO}}/src/**", &ctx),
            "/project/src/**"
        );
        assert_eq!(
            preprocess_config_pattern("{{TMPDIR}}/temp/**", &ctx),
            "/tmp/temp/**"
        );
        assert_eq!(preprocess_config_pattern("/home/user/", &ctx), "/home/user");
        // Relative patterns resolve against the cwd.
        assert_eq!(
            preprocess_config_pattern("rel/file", &ctx),
            "/project/rel/file"
        );
    }

    #[test]
    fn unset_env_vars_stay_literal() {
        let ctx = posix_ctx();
        let pattern = preprocess_config_pattern("$TABIT_GATE_UNSET_VAR_XYZ/file", &ctx);
        assert!(pattern.contains("$TABIT_GATE_UNSET_VAR_XYZ"), "{pattern}");
    }

    #[test]
    fn win32_drive_forms_canonicalize() {
        let ctx = win_ctx();
        assert_eq!(
            preprocess_config_pattern("C:\\Users\\file", &ctx),
            "/c/Users/file"
        );
        assert_eq!(preprocess_config_pattern("C:/", &ctx), "/c");
        assert_eq!(preprocess_config_pattern("C:\\", &ctx), "/c");
        assert_eq!(canonicalize_drive("C:"), "/c");
        assert_ne!(
            preprocess_config_pattern("C:\\data", &ctx),
            preprocess_config_pattern("D:\\data", &ctx)
        );
        assert_eq!(preprocess_config_pattern("C:/", &win_ctx()), "/c");
    }

    #[test]
    fn canonicalize_drive_is_idempotent() {
        assert_eq!(canonicalize_drive("C:/Users"), "/c/Users");
        assert_eq!(canonicalize_drive("d:/data"), "/d/data");
        assert_eq!(canonicalize_drive("/c/Users"), "/c/Users");
        assert_eq!(canonicalize_drive("/etc/passwd"), "/etc/passwd");
        assert_eq!(canonicalize_drive("//server/share"), "//server/share");
    }

    #[test]
    fn git_bash_input_converts_on_win32_only() {
        assert_eq!(
            preprocess_runtime_path("/c/Users/file", &win_ctx()),
            "/c/Users/file"
        );
        assert_eq!(
            preprocess_runtime_path("/c/Users/file", &posix_ctx()),
            "/c/Users/file"
        );
    }

    #[test]
    fn expand_tilde_only_at_word_start() {
        assert_eq!(expand_tilde("~", "/h"), "/h");
        assert_eq!(expand_tilde("~/x", "/h"), "/h/x");
        assert_eq!(expand_tilde("~\\x", "/h"), "/h\\x");
        assert_eq!(expand_tilde("~user", "/h"), "~user");
        assert_eq!(expand_tilde("abc~def", "/h"), "abc~def");
    }

    #[test]
    fn node_posix_normalization() {
        assert_eq!(node_path::posix_normalize(""), ".");
        assert_eq!(node_path::posix_normalize("/a//b/"), "/a/b/");
        assert_eq!(node_path::posix_normalize("a/.."), ".");
        assert_eq!(node_path::posix_normalize("/a/.."), "/");
        assert_eq!(node_path::posix_normalize("./x/../y"), "y");
        assert_eq!(
            node_path::posix_resolve("/project", "file"),
            "/project/file"
        );
        assert_eq!(node_path::posix_resolve("/", "file"), "/file");
        assert_eq!(node_path::posix_resolve("/a/b", "/c"), "/c");
    }

    #[test]
    fn node_win32_normalization_and_resolution() {
        assert_eq!(node_path::win32_normalize("C:/a/b/../c"), "C:\\a\\c");
        assert_eq!(node_path::win32_normalize("C:/"), "C:\\");
        assert_eq!(node_path::win32_normalize("C:"), "C:\\");
        assert_eq!(node_path::win32_normalize("c:\\a\\b\\"), "c:\\a\\b\\");
        assert_eq!(
            node_path::win32_normalize("//server/share"),
            "\\\\server\\share"
        );
        assert_eq!(node_path::win32_normalize("a/.."), ".");
        assert!(node_path::win32_is_absolute("C:\\a"));
        assert!(!node_path::win32_is_absolute("C:foo"));
        assert!(node_path::win32_is_absolute("/foo"));
        assert_eq!(
            node_path::win32_resolve("C:\\Windows", "sysfile"),
            "C:\\Windows\\sysfile"
        );
        assert_eq!(node_path::win32_resolve("C:\\a", "/foo"), "C:\\foo");
        assert_eq!(node_path::win32_resolve("C:\\a", "C:foo"), "C:\\a\\foo");
        assert_eq!(node_path::win32_resolve("D:\\x", "C:foo"), "C:\\foo");
        assert_eq!(
            node_path::win32_join(&["C:\\Temp", "a\\b"]),
            "C:\\Temp\\a\\b"
        );
        assert_eq!(node_path::win32_join(&["C:\\Temp"]), "C:\\Temp");
    }
}
