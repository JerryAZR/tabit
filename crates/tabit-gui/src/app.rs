//! The eframe app: display state + view. Thin by contract — every
//! decision lives in the reducer; this file only projects
//! [`GuiState`] to widgets and routes input back out.

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;

use crate::backend::{self, Backend};
use crate::reducer::{Group, GuiState, InMsg, Phase, Segment};
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
        };
        app.start_backend(ctx);
        app
    }

    /// Spawn the backend with `--continue`: returning users get their
    /// newest session; an empty store is absorbed backend-side into a
    /// fresh start (the ack's `resumed: false` carries the note). A
    /// spawn failure is the environment refusing — shown with the OS
    /// reason and the reinstall hint.
    fn start_backend(&mut self, ctx: egui::Context) {
        let cwd = self.cwd.clone();
        let tabit = self.tabit.clone();
        match backend::spawn(cwd.as_deref(), tabit.as_deref(), move || {
            ctx.request_repaint()
        }) {
            Ok(backend) => self.backend = Some(backend),
            Err(error) => {
                self.state.phase = Phase::Exited {
                    clean: false,
                    reason: format!(
                        "could not start the backend: {error}. If it persists, reinstall tabit"
                    ),
                };
            }
        }
    }

    /// Restart the backend — the manual reload and the only retry: a
    /// respawn re-reads config, auth, and sessions from disk (fix the
    /// file, click, the fresh handshake reflects it). The GUI never
    /// respawns on its own — every death is explained on screen, and
    /// the user's click is the rate limiter.
    fn restart(&mut self, ctx: egui::Context) {
        self.backend = None;
        self.state = GuiState::default();
        self.start_backend(ctx);
    }

    /// Send the input box — the one choke point. Text leaves the box only
    /// over a Live channel: every affordance (button, Enter) routes here,
    /// so none can bypass the guard and silently lose input while the
    /// backend is gone (v1 has no send acknowledgment to catch it with).
    fn send(&mut self) {
        if self.state.phase != Phase::Live {
            return;
        }
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
    /// logic without a paint pass). Pure folding — no lifecycle
    /// decisions live here; deaths are classified by the reducer and
    /// every retry is the user's click.
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let msgs = self.backend.as_ref().map(|b| b.drain()).unwrap_or_default();
        for msg in msgs {
            // An internal-error crash auto-opens the stderr disclosure:
            // the report is the payload, not a hidden diagnostic.
            if matches!(&msg, InMsg::BackendExited { code: Some(101) }) {
                self.display.show_stderr = true;
            }
            self.state.reduce(msg);
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
                    } else if self.state.facts.is_none() {
                        // No session was ever established (spawn
                        // failure): the action is a plain retry.
                        "retry"
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
            for segment in &turn.segments {
                match segment {
                    Segment::Reasoning { text, .. } => lines += text_lines(text),
                    Segment::Text(text) => lines += text_lines(text),
                    Segment::ToolCall(_) => lines += 1.0,
                }
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
            // Arrival order, exactly as the wire interleaved it.
            for segment in &turn.segments {
                match segment {
                    Segment::Reasoning { text, .. } => {
                        ui.horizontal(|ui| {
                            ui.add_space(theme::ROW_INSET);
                            ui.label(
                                egui::RichText::new(format!("thinking: {text}"))
                                    .color(theme::MUTED)
                                    .italics(),
                            );
                        });
                    }
                    Segment::Text(text) => {
                        ui.horizontal(|ui| {
                            ui.add_space(theme::ROW_INSET);
                            ui.label(egui::RichText::new(text.clone()).color(theme::TEXT));
                        });
                    }
                    Segment::ToolCall(tool) => {
                        ui.horizontal(|ui| {
                            ui.add_space(theme::ROW_INSET);
                            let mark = if tool.done { "✓" } else { "…" };
                            ui.label(
                                egui::RichText::new(format!("{mark} {}", tool.name))
                                    .color(theme::MUTED),
                            );
                        });
                    }
                }
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
