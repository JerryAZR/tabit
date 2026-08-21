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
    let cwd = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui_opts(),
        ..Default::default()
    };
    eframe::run_native(
        "tabit",
        options,
        Box::new(|cc| Ok(Box::new(app::TabitApp::new(cwd, cc.egui_ctx.clone())))),
    )
}

fn egui_opts() -> egui::ViewportBuilder {
    use egui::ViewportBuilder;
    ViewportBuilder::default()
        .with_inner_size([1000.0, 700.0])
        .with_min_inner_size([480.0, 320.0])
}
