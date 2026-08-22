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
    let (cwd, launcher_tabit) = parse_args();
    let tabit = launcher_tabit.unwrap_or_else(|| {
        // The launcher passes `--tabit`; these fallbacks serve direct
        // `cargo run -p tabit-gui` development only.
        if let Ok(path) = std::env::var("TABIT_BIN") {
            return PathBuf::from(path);
        }
        std::env::current_exe()
            .ok()
            .and_then(|exe| {
                exe.parent()
                    .map(|dir| dir.join(format!("tabit{}", std::env::consts::EXE_SUFFIX)))
            })
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("tabit"))
    });
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
                Some(tabit),
                cc.egui_ctx.clone(),
            )))
        }),
    )
}

/// The GUI's own two flags: an optional project directory (positional)
/// and `--tabit <path>` (the launcher's exact backend executable).
fn parse_args() -> (Option<PathBuf>, Option<PathBuf>) {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I>(args: I) -> (Option<PathBuf>, Option<PathBuf>)
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
                tabit = args.next().map(PathBuf::from);
            }
            other if cwd.is_none() => cwd = Some(PathBuf::from(other)),
            _ => {}
        }
    }
    (cwd, tabit)
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
        let (cwd, tabit) = parse_args_from(["--tabit", "C:/bin/tabit.exe", "."]);
        assert_eq!(cwd.as_deref(), Some(std::path::Path::new(".")));
        assert_eq!(
            tabit.as_deref(),
            Some(std::path::Path::new("C:/bin/tabit.exe"))
        );
        let (cwd, tabit) = parse_args_from([""; 0]);
        assert_eq!(cwd, None);
        assert_eq!(tabit, None);
    }
}
