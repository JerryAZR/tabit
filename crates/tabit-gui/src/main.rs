//! The tabit GUI: one window, many sessions, one active view — one
//! `tabit-core --json` child (the multi-session host).
//!
//! The optional argument is the project directory to run the backend
//! in; the backend binary is resolved as this executable's sibling
//! `tabit-core` (or the `TABIT_CORE_BIN` development override).

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
    let cwd = match parse_args() {
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
        Box::new(move |cc| Ok(Box::new(app::TabitApp::new(cwd, cc.egui_ctx.clone())))),
    )
}

/// The GUI's one flag: an optional project directory (positional).
/// Strict — anything unexpected is a loud error, never a silent no-op.
fn parse_args() -> Result<Option<PathBuf>, String> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I>(args: I) -> Result<Option<PathBuf>, String>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut cwd = None;
    for arg in args.into_iter().map(Into::into) {
        if arg.starts_with('-') {
            return Err(format!("unknown flag `{arg}`"));
        }
        if cwd.is_some() {
            return Err(format!(
                "unexpected second argument `{arg}` — one project path"
            ));
        }
        cwd = Some(PathBuf::from(arg));
    }
    Ok(cwd)
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
    fn project_path_parses_positionally() {
        let cwd = parse_args_from(["."]).expect("valid launch");
        assert_eq!(cwd.as_deref(), Some(std::path::Path::new(".")));
        // Bare: no path, the backend runs in the GUI's cwd.
        let cwd = parse_args_from([""; 0]).expect("bare");
        assert_eq!(cwd, None);
    }

    #[test]
    fn unexpected_args_are_loud_errors() {
        // Unknown flags and a second positional are user mistakes,
        // not silent no-ops.
        assert!(parse_args_from(["--bogus"]).is_err());
        assert!(parse_args_from([".", "extra"]).is_err());
    }
}
