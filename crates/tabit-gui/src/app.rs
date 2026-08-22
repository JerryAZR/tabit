//! The eframe app: display state + view. Thin by contract — every
//! decision lives in the reducer; this file only projects
//! [`GuiState`] to widgets and routes input back out.

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;

use crate::backend::{self, Backend};
use crate::reducer::{Group, GuiState, Phase};
use crate::theme;

/// View-only state, expected to churn on the polish pass — kept out
/// of the reducer by contract.
#[derive(Default)]
struct Display {
    input: String,
    /// Follow the transcript tail; pauses when the user scrolls up,
    /// re-pins at the bottom.
    pinned: bool,
    /// Content height at the last frame — detects growth while
    /// pinned (tail-follow).
    content_bottom: f32,
    /// Crash-report toggle.
    show_stderr: bool,
}

pub struct TabitApp {
    state: GuiState,
    display: Display,
    backend: Option<Backend>,
    cwd: Option<PathBuf>,
    /// The exact backend executable, handed over by the launcher.
    tabit: Option<PathBuf>,
    /// The automatic no-sessions fallback fires at most once per
    /// backend lifetime — a startup that keeps failing must stop and
    /// show the banner, never spin (owner report: respawn loop).
    /// Manual restarts reset it.
    auto_fresh_used: bool,
}

impl TabitApp {
    pub fn new(cwd: Option<PathBuf>, tabit: Option<PathBuf>, ctx: egui::Context) -> Self {
        theme::apply(&ctx);
        let mut app = Self {
            state: GuiState::default(),
            display: Display {
                pinned: true,
                ..Default::default()
            },
            backend: None,
            cwd,
            tabit,
            auto_fresh_used: false,
        };
        app.start_backend(true, ctx);
        app
    }

    fn start_backend(&mut self, resume_newest: bool, ctx: egui::Context) {
        let cwd = self.cwd.clone();
        let tabit = self.tabit.clone();
        match backend::spawn(cwd.as_deref(), tabit.as_deref(), resume_newest, move || {
            ctx.request_repaint()
        }) {
            Ok(backend) => self.backend = Some(backend),
            Err(error) => {
                self.state.phase = Phase::Exited {
                    clean: false,
                    reason: format!("could not spawn the tabit backend: {error}"),
                };
            }
        }
    }

    /// Restart the backend — the manual reload: a respawn re-reads
    /// config, auth, and sessions from disk (fix the file, click, the
    /// fresh handshake reflects it). Always attempt `--continue`; the
    /// no-sessions fallback covers a fresh install, and the
    /// resume-vs-fresh guesswork stays out of the GUI.
    fn restart(&mut self, ctx: egui::Context) {
        self.backend = None;
        self.state = GuiState::default();
        self.auto_fresh_used = false;
        self.start_backend(true, ctx);
    }

    fn send(&mut self) {
        let text = self.display.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.display.input.clear();
        self.display.pinned = true;
        if let Some(backend) = &self.backend {
            backend.send_message(&text);
            self.state.message_sent(text);
        }
    }

    fn abort(&mut self) {
        if let Some(backend) = &self.backend
            && self.state.running
        {
            backend.abort();
        }
    }
}

impl eframe::App for TabitApp {
    /// The no-UI half of the contract: fold backend messages into
    /// state (also runs while the window is hidden — eframe calls
    /// logic without a paint pass).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let msgs = self.backend.as_ref().map(|b| b.drain()).unwrap_or_default();
        for msg in msgs {
            let was_connecting = self.state.phase == Phase::Connecting;
            self.state.reduce(msg);
            // `--continue` with no sessions gets exactly one fresh
            // respawn; rejections and repeated failures are never
            // retried automatically — they land in the banner with
            // the manual reload/restart buttons.
            if was_connecting
                && self.state.facts.is_none()
                && matches!(self.state.phase, Phase::Exited { .. })
                && !self.state.handshake_rejected
                && !self.auto_fresh_used
            {
                self.auto_fresh_used = true;
                self.backend = None;
                self.state = GuiState::default();
                self.start_backend(false, ctx.clone());
                return;
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 2. Status strip (reads hoisted: the closure needs `&mut
        // self` only for actions).
        let phase = self.state.phase.clone();
        let facts = self.state.facts.clone();
        let pending = self.state.pending.len();
        let usage = self.state.usage;
        let stderr_tail = self
            .backend
            .as_ref()
            .map(|b| b.stderr_tail())
            .unwrap_or_default();
        egui::containers::Panel::top("status").show(ui, |ui| {
            ui.horizontal(|ui| match &phase {
                Phase::Connecting => {
                    ui.label("connecting…");
                }
                Phase::Live => {
                    let dot = if self.state.running {
                        egui::RichText::new("●").color(theme::ACCENT)
                    } else {
                        egui::RichText::new("○").color(theme::MUTED)
                    };
                    ui.label(dot);
                    if let Some(facts) = &facts {
                        let selection = match &facts.model.thinking_level {
                            Some(level) => format!(
                                "{} / {} ({level})",
                                facts.model.provider, facts.model.model
                            ),
                            None => format!("{} / {}", facts.model.provider, facts.model.model),
                        };
                        ui.label(egui::RichText::new(selection).color(theme::MUTED));
                    }
                    if pending > 0 {
                        ui.label(
                            egui::RichText::new(format!("{pending} queued")).color(theme::MUTED),
                        );
                    }
                    let usage = &usage;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "↑{} ↓{}",
                                usage.input_tokens, usage.output_tokens
                            ))
                            .color(theme::MUTED),
                        );
                    });
                }
                Phase::Exited { clean, reason } => {
                    let color = if *clean { theme::MUTED } else { theme::ERROR };
                    ui.label(egui::RichText::new(reason.clone()).color(color));
                    let action = if self.state.handshake_rejected {
                        "reload config / retry"
                    } else {
                        "restart session"
                    };
                    if ui.button(action).clicked() {
                        self.restart(ctx.clone());
                    }
                    if !*clean && !stderr_tail.is_empty() {
                        let toggle = if self.display.show_stderr {
                            "stderr ▴"
                        } else {
                            "stderr ▾"
                        };
                        if ui.button(toggle).clicked() {
                            self.display.show_stderr = !self.display.show_stderr;
                        }
                    }
                }
            });
            if self.display.show_stderr && matches!(phase, Phase::Exited { clean: false, .. }) {
                for line in &stderr_tail {
                    ui.label(
                        egui::RichText::new(line.clone())
                            .color(theme::MUTED)
                            .small(),
                    );
                }
            }
        });

        // 3. Input.
        egui::containers::Panel::bottom("input").show(ui, |ui| {
            ui.horizontal(|ui| {
                let send = ui.add(
                    egui::TextEdit::singleline(&mut self.display.input)
                        .hint_text("message the agent — mid-run messages steer"),
                );
                if send.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.send();
                    send.request_focus();
                }
                if self.state.running {
                    if ui.button("abort").clicked() {
                        self.abort();
                    }
                } else if ui
                    .add_enabled(self.state.phase == Phase::Live, egui::Button::new("send"))
                    .clicked()
                {
                    self.send();
                }
            });
        });

        // 4. Transcript, virtualized through the viewport pattern.
        egui::CentralPanel::default().show(ui, |ui| {
            let line_h = ui.text_style_height(&egui::TextStyle::Body);
            let width = ui.available_width();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show_viewport(ui, |ui, viewport| {
                    let mut y = 0.0;
                    for group in &self.state.transcript {
                        let estimated = estimate_height(group, width, line_h);
                        let visible = y + estimated >= viewport.top() - 500.0
                            && y <= viewport.bottom() + 500.0;
                        if visible {
                            ui.add_space(theme::ROW_GAP);
                            render_group(ui, group);
                        } else {
                            ui.add_space(estimated + theme::ROW_GAP);
                        }
                        y += estimated + theme::ROW_GAP;
                    }
                    for text in &self.state.pending {
                        ui.add_space(theme::ROW_GAP);
                        ui.label(
                            egui::RichText::new(format!("queued: {text}"))
                                .color(theme::MUTED)
                                .italics(),
                        );
                    }
                    // Tail-follow: re-pins near the bottom, follows
                    // growth while pinned.
                    let grew = y > self.display.content_bottom;
                    self.display.pinned =
                        viewport.bottom() >= y - 4.0 * line_h || self.display.pinned && grew;
                    if self.display.pinned && grew {
                        ui.scroll_to_cursor(Some(egui::Align::Max));
                    }
                    self.display.content_bottom = y;
                });
        });

        // 5. Keep frames coming while anything is in flight.
        if self.state.running || self.state.phase == Phase::Connecting {
            self_repaint(&ctx);
        }
    }
}

/// Estimated rendered height of one group — the virtualization
/// approximation; rows near the viewport render and self-correct.
fn estimate_height(group: &Group, width: f32, line_h: f32) -> f32 {
    let text_lines = |text: &str| -> f32 {
        let chars_per_line = ((width - 2.0 * theme::ROW_INSET) / (line_h * 0.55)).max(1.0) as usize;
        text.len().div_ceil(chars_per_line).max(1) as f32
    };
    match group {
        Group::User { text } => text_lines(text) * line_h,
        Group::Turn(turn) => {
            let mut lines = 0.0;
            for block in &turn.reasoning {
                lines += text_lines(&block.text);
            }
            lines += turn.tools.len() as f32;
            if !turn.text.is_empty() {
                lines += text_lines(&turn.text);
            }
            lines * line_h
        }
        Group::Notice { text, .. } => text_lines(text) * line_h,
        Group::Native { item } => text_lines(item) * line_h,
    }
}

fn self_repaint(ctx: &egui::Context) {
    ctx.request_repaint_after(Duration::from_millis(100));
}

fn render_group(ui: &mut egui::Ui, group: &Group) {
    match group {
        Group::User { text } => {
            ui.horizontal(|ui| {
                ui.add_space(theme::ROW_INSET);
                ui.label(egui::RichText::new(format!("you: {text}")).color(theme::USER_TEXT));
            });
        }
        Group::Turn(turn) => {
            for block in &turn.reasoning {
                ui.horizontal(|ui| {
                    ui.add_space(theme::ROW_INSET);
                    ui.label(
                        egui::RichText::new(format!("thinking: {}", block.text))
                            .color(theme::MUTED)
                            .italics(),
                    );
                });
            }
            for tool in &turn.tools {
                ui.horizontal(|ui| {
                    ui.add_space(theme::ROW_INSET);
                    let mark = if tool.done { "✓" } else { "…" };
                    ui.label(
                        egui::RichText::new(format!("{mark} {}", tool.name)).color(theme::MUTED),
                    );
                });
            }
            if !turn.text.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(theme::ROW_INSET);
                    ui.label(egui::RichText::new(turn.text.clone()).color(theme::TEXT));
                });
            }
        }
        Group::Notice { text, error } => {
            let color = if *error { theme::ERROR } else { theme::MUTED };
            ui.horizontal(|ui| {
                ui.add_space(theme::ROW_INSET);
                ui.label(egui::RichText::new(text.clone()).color(color));
            });
        }
        Group::Native { item } => {
            ui.horizontal(|ui| {
                ui.add_space(theme::ROW_INSET);
                ui.label(
                    egui::RichText::new(format!("native item: {item}"))
                        .color(theme::MUTED)
                        .small(),
                );
            });
        }
    }
}
