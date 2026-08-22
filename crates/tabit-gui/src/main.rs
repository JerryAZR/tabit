//! The tabit GUI: one window, one session, one `tabit --json` child.
//!
//! Users do not run this binary directly — the `tabit` launcher
//! spawns it detached (`tabit [path]`, ROADMAP item 7). The optional
//! argument is the project directory to run the backend in;
//! `TABIT_BIN` overrides the backend binary for development.

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

mod app;
mod backend;
mod reducer;
mod theme;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let (cwd, tabit) = match parse_args() {
        Ok(parsed) => parsed,
        Err(usage) => {
            eprintln!("tabit-gui: {usage}");
            std::process::exit(2);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui_opts(),
        ..Default::default()
    };
    eframe::run_native(
        "tabit",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::TabitApp::new(
                cwd,
                tabit,
                cc.egui_ctx.clone(),
            )))
        }),
    )
}

/// The GUI's own two flags: an optional project directory (positional)
/// and `--tabit <path>` (the launcher's exact backend executable).
/// Strict — anything unexpected is a loud error, never a silent no-op.
fn parse_args() -> Result<(Option<PathBuf>, Option<PathBuf>), String> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I>(args: I) -> Result<(Option<PathBuf>, Option<PathBuf>), String>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut cwd = None;
    let mut tabit = None;
    let mut args = args.into_iter().map(Into::into);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tabit" => {
                let value = args.next().ok_or("--tabit needs a path")?;
                tabit = Some(PathBuf::from(value));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`"));
            }
            positional => {
                if cwd.is_some() {
                    return Err(format!(
                        "unexpected second argument `{positional}` — one project path"
                    ));
                }
                cwd = Some(PathBuf::from(positional));
            }
        }
    }
    Ok((cwd, tabit))
}

fn egui_opts() -> egui::ViewportBuilder {
    use egui::ViewportBuilder;
    ViewportBuilder::default()
        .with_inner_size([1000.0, 700.0])
        .with_min_inner_size([480.0, 320.0])
}

#[cfg(test)]
mod tests {
    use super::parse_args_from;

    #[test]
    fn launcher_args_parse() {
        let (cwd, tabit) =
            parse_args_from(["--tabit", "C:/bin/tabit.exe", "."]).expect("valid launch");
        assert_eq!(cwd.as_deref(), Some(std::path::Path::new(".")));
        assert_eq!(
            tabit.as_deref(),
            Some(std::path::Path::new("C:/bin/tabit.exe"))
        );
        // Dev mode: no launcher, no path — backend.rs resolves the binary.
        let (cwd, tabit) = parse_args_from([""; 0]).expect("bare");
        assert_eq!(cwd, None);
        assert_eq!(tabit, None);
    }

    #[test]
    fn unexpected_args_are_loud_errors() {
        // A missing --tabit value, unknown flags, and a second
        // positional are user mistakes, not silent no-ops.
        assert!(parse_args_from(["--tabit"]).is_err());
        assert!(parse_args_from(["--bogus"]).is_err());
        assert!(parse_args_from([".", "extra"]).is_err());
        assert!(parse_args_from(["--tabit", "t", ".", "extra"]).is_err());
    }
}
