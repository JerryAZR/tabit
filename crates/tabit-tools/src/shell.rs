//! The shell choice for the shell tools — decided once per process, at
//! registration, never per invocation.
//!
//! Correctness over coverage (owner ruling, 2026-09): only a positively
//! identified Git-for-Windows install yields the `bash` tool; every miss
//! on Windows yields the `powershell` tool. A wrong bash — WSL's
//! `System32\bash.exe` launcher, a Cygwin/MSYS2 root with different path
//! mapping — is worse than no bash, so there is deliberately no
//! bare-`bash.exe`-on-PATH source. Identification sources, in trust order:
//!
//! 1. the `git.exe` on PATH (the install the user actually runs), accepted
//!    only in Git-for-Windows placements — `<root>\cmd\git.exe` or
//!    `<root>\mingw64\bin\git.exe`;
//! 2. the installer's registry declaration, `SOFTWARE\GitForWindows`'
//!    `InstallPath` (per-user `HKCU` before machine `HKLM` — also the
//!    authoritative system-vs-user answer);
//! 3. the installer's default directories (per-user before machine).
//!
//! The first surviving candidate is spawn-probed: a present-but-broken
//! bash registers `powershell` instead of a tool that fails every call.

/// The interpreter a shell-tool invocation runs through.
pub(crate) struct Interpreter {
    pub(crate) argv0: String,
    /// Flags before the command text.
    pub(crate) args: &'static [&'static str],
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    /// Flags before the command text for the bash dialect.
    const BASH_ARGS: &[&str] = &["-c"];
    /// Flags for the PowerShell dialect: no profile, one command text.
    /// pi's hardening (stolen 2026-09): `-NonInteractive` turns a
    /// would-be prompt into an immediate failure instead of a hang to
    /// the timeout; `-ExecutionPolicy Bypass` keeps locked-down
    /// machines from refusing the session (process-local only — the
    /// machine policy is untouched).
    const POWERSHELL_ARGS: &[&str] = &[
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
    ];

    /// Cap on the registration-time spawn probe: a bash that cannot say
    /// `exit 0` within two seconds is not a bash worth registering.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
    const REGISTRY_SUBKEY: &str = "SOFTWARE\\GitForWindows";
    /// CREATE_NO_WINDOW for the registration-time probes: they spawn
    /// console binaries (`where.exe`, `bash.exe`) from a console-less
    /// backend, and without the flag each one flashes a terminal window
    /// (run_shell's shim covers the tool calls; these are the one-shot
    /// lookups).
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    static RESOLVED: OnceLock<Shell> = OnceLock::new();

    /// This machine's shell. [`Shell::Bash`] only ever holds a
    /// probe-verified Git-for-Windows `bash.exe`.
    #[derive(Clone, Debug)]
    pub(crate) enum Shell {
        Bash(PathBuf),
        Powershell,
    }

    pub(crate) fn resolved() -> &'static Shell {
        RESOLVED.get_or_init(resolve)
    }

    /// The interpreter for the `bash` tool. `Err` — never a silent
    /// PowerShell substitution — keeps the dialect honest: a
    /// bash-dialect command must not run under PowerShell.
    pub(crate) fn bash() -> Result<Interpreter, String> {
        match resolved() {
            Shell::Bash(path) => Ok(Interpreter {
                argv0: path.to_string_lossy().into_owned(),
                args: BASH_ARGS,
            }),
            Shell::Powershell => Err(
                "this machine has no verified Git Bash — commands here run through the powershell tool"
                    .to_string(),
            ),
        }
    }

    /// The interpreter for the `powershell` tool. PowerShell is an OS
    /// component; it is the floor, with nothing below it to check.
    pub(crate) fn powershell() -> Interpreter {
        Interpreter {
            argv0: "powershell".to_string(),
            args: POWERSHELL_ARGS,
        }
    }

    fn resolve() -> Shell {
        for root in candidate_roots() {
            let Some(bash) = git_bash_exe(&root) else {
                continue;
            };
            if spawn_probe(&bash, &["-c", "exit 0"], PROBE_TIMEOUT) {
                return Shell::Bash(bash);
            }
        }
        Shell::Powershell
    }

    /// Install-root candidates in trust order (see the module docs).
    pub(crate) fn candidate_roots() -> Vec<PathBuf> {
        let mut roots = git_exe_roots();
        roots.extend(registry_roots());
        roots.extend(well_known_roots());
        roots
    }

    /// Roots derived from the `git.exe` locations PATH reports. `where`
    /// prints one match per line in PATH order; the shape filter below
    /// keeps only Git-for-Windows placements.
    fn git_exe_roots() -> Vec<PathBuf> {
        let mut where_git = std::process::Command::new("where.exe");
        let Ok(output) = where_git
            .arg("git")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter_map(|line| root_from_git_exe(Path::new(line)))
            .collect()
    }

    /// The Git-for-Windows root implied by a `git.exe` location:
    /// `<root>\cmd\git.exe` (the installer's PATH entry) or
    /// `<root>\mingw64\bin\git.exe` (a Git Bash session's PATH). Any other
    /// placement — MSYS2/Cygwin's `usr\bin\git.exe`, a scoop shim — is not
    /// Git for Windows.
    fn root_from_git_exe(git_exe: &Path) -> Option<PathBuf> {
        let parent = git_exe.parent()?;
        match parent.file_name()?.to_str()?.to_ascii_lowercase().as_str() {
            "cmd" => parent.parent().map(Path::to_path_buf),
            "bin" => {
                let mingw = parent.parent()?;
                if mingw.file_name()?.to_str()?.eq_ignore_ascii_case("mingw64") {
                    mingw.parent().map(Path::to_path_buf)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// The installer's own declaration. `HKCU` before `HKLM`: with both
    /// present, the per-user install is the one chosen for this account.
    /// A stale or missing key just falls through to the next source.
    fn registry_roots() -> Vec<PathBuf> {
        [
            (&windows_registry::CURRENT_USER, "per-user"),
            (&windows_registry::LOCAL_MACHINE, "machine"),
        ]
        .into_iter()
        .filter_map(|(hive, _)| hive.open(REGISTRY_SUBKEY).ok())
        .filter_map(|key| key.get_string("InstallPath").ok())
        .map(PathBuf::from)
        .collect()
    }

    /// The installer's default directories, per-user before machine — for
    /// installs with git off PATH and no registry marker.
    fn well_known_roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Programs").join("Git"));
        }
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(var) {
                roots.push(PathBuf::from(dir).join("Git"));
            }
        }
        roots
    }

    /// The bash of a Git-for-Windows root: `bin\bash.exe` only — the
    /// self-initializing launcher (the why lives in the comment below).
    fn git_bash_exe(root: &Path) -> Option<PathBuf> {
        // `bin\bash.exe` only, deliberately no `usr\bin\bash.exe`
        // fallback: bin initializes the full Git Bash environment from
        // its own location — PATH with coreutils, HOME, the MSYS setup —
        // even from an empty environment (verified: `env -i` still runs
        // `ls`), while the raw usr\bin bash inherits the launcher's
        // environment wholesale, which is exactly the bug class this
        // module exists to prevent (pi launches bin\bash.exe for the
        // same reason).
        let bin = root.join("bin").join("bash.exe");
        bin.is_file().then_some(bin)
    }

    /// Run one throwaway command and see it exit cleanly — the health gate
    /// between "a bash file exists" and "this machine has bash".
    /// Force-killed at `timeout` (a hung binary is not usable either).
    pub(crate) fn spawn_probe(program: &Path, args: &[&str], timeout: Duration) -> bool {
        let Ok(mut child) = std::process::Command::new(program)
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return false,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_resolved_bash_is_the_self_initializing_launcher() {
            // The regression this pins: a launcher whose PATH carries
            // `Git\cmd` but not `usr\bin` made every coreutils call
            // fail (`ls: command not found`) under the raw usr\bin
            // bash. Only the self-initializing bin\bash.exe resolves.
            let interpreter = bash().expect("a verified bash exists on this machine");
            assert!(
                interpreter
                    .argv0
                    .replace('/', "\\")
                    .ends_with(r"\bin\bash.exe"),
                "{:?}",
                interpreter.argv0
            );
        }

        #[test]
        fn git_exe_placements_map_to_roots() {
            assert_eq!(
                root_from_git_exe(Path::new(r"C:\Program Files\Git\cmd\git.exe")),
                Some(Path::new(r"C:\Program Files\Git").to_path_buf())
            );
            assert_eq!(
                root_from_git_exe(Path::new(r"C:\Git\mingw64\bin\git.exe")),
                Some(Path::new(r"C:\Git").to_path_buf())
            );
            // MSYS2/Cygwin layouts and package-manager shims are rejected
            // by shape — a wrong bash is worse than none.
            assert_eq!(
                root_from_git_exe(Path::new(r"C:\msys64\usr\bin\git.exe")),
                None
            );
            assert_eq!(
                root_from_git_exe(Path::new(r"C:\Users\j\scoop\shims\git.exe")),
                None
            );
            assert_eq!(root_from_git_exe(Path::new("git.exe")), None);
        }

        #[test]
        fn git_bash_exe_requires_the_bin_launcher() {
            let dir =
                std::env::temp_dir().join(format!("tabit-shell-tests-{}", std::process::id()));

            // The raw usr\bin bash is not a usable bash: no fallback.
            let with_usr = dir.join("usr-root");
            std::fs::create_dir_all(with_usr.join("usr").join("bin")).expect("dirs");
            std::fs::write(with_usr.join("usr").join("bin").join("bash.exe"), b"").expect("write");
            assert_eq!(git_bash_exe(&with_usr), None);

            // The self-initializing bin launcher is the one bash.
            let with_bin = dir.join("bin-root");
            std::fs::create_dir_all(with_bin.join("bin")).expect("dirs");
            std::fs::write(with_bin.join("bin").join("bash.exe"), b"").expect("write");
            assert_eq!(
                git_bash_exe(&with_bin),
                Some(with_bin.join("bin").join("bash.exe"))
            );

            let empty = dir.join("empty-root");
            std::fs::create_dir_all(&empty).expect("dirs");
            assert_eq!(git_bash_exe(&empty), None);
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn spawn_probe_classifies_clean_broken_and_hung() {
            // PowerShell stands in for the probe's program: an OS
            // component always present on the test machine. The
            // deadlines on the classification cases are bounds, not
            // behavior — a cold CLR start on a loaded CI runner takes
            // several seconds, so they get margin; only the hung case
            // exercises the deadline path, and it stays tight.
            let bound = Duration::from_secs(15);
            assert!(spawn_probe(
                Path::new("powershell"),
                &["-NoProfile", "-Command", "exit 0"],
                bound
            ));
            assert!(!spawn_probe(
                Path::new("powershell"),
                &["-NoProfile", "-Command", "exit 3"],
                bound
            ));
            assert!(!spawn_probe(
                Path::new("definitely-not-a-program-xyz"),
                &[],
                bound
            ));
            // A short deadline keeps the hung case fast; the sleep runs
            // inside the probed process, so the kill leaves no orphan.
            assert!(!spawn_probe(
                Path::new("powershell"),
                &["-NoProfile", "-Command", "Start-Sleep -Seconds 30"],
                Duration::from_millis(200)
            ));
        }

        #[test]
        fn resolved_shell_is_never_a_guess() {
            match resolved() {
                Shell::Bash(path) => {
                    assert!(
                        path.is_file(),
                        "resolved bash must exist: {}",
                        path.display()
                    );
                    assert!(
                        path.file_name()
                            .is_some_and(|n| n.eq_ignore_ascii_case("bash.exe"))
                    );
                }
                // A machine without a verified Git Bash — the safe answer,
                // not a failure.
                Shell::Powershell => {}
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use windows::{Shell, bash, powershell, resolved};

#[cfg(not(windows))]
mod unix {
    use super::*;

    /// Flags before the command text for the bash dialect.
    const BASH_ARGS: &[&str] = &["-c"];

    /// Unix assumes bash outright: it is a hard dependency of the POSIX
    /// world, and a missing one fails loudly per invocation rather than
    /// being papered over at registration.
    pub(crate) fn bash() -> Result<Interpreter, String> {
        Ok(Interpreter {
            argv0: "bash".to_string(),
            args: BASH_ARGS,
        })
    }
}

#[cfg(not(windows))]
pub(crate) use unix::bash;
