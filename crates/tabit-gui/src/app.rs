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
    /// Free-text drafts per open interaction card id.
    answers: std::collections::HashMap<String, String>,
    /// Measured rendered height per transcript group (the
    /// virtualization's spacing for off-screen rows; estimates are
    /// provisional until a group first renders — measured heights keep
    /// the scroll geometry stable, so the bottom stays where it is).
    heights: Vec<Option<f32>>,
    /// The session the height cache belongs to; a new session (or a
    /// respawn replaying a different chain) invalidates it.
    session: Option<String>,
    /// "New session" is armed after the first click (a second click
    /// confirms; anything else disarms).
    confirm_new: bool,
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
            display: Display::default(),
            backend: None,
            cwd,
            tabit,
        };
        app.start_backend(ctx, true);
        app
    }

    /// Spawn the backend. `resume` reattaches to the newest session
    /// (`--continue`); a fresh start is the GUI's "new session" path.
    /// A spawn failure is the environment refusing — shown with the OS
    /// reason and the reinstall hint.
    fn start_backend(&mut self, ctx: egui::Context, resume: bool) {
        let cwd = self.cwd.clone();
        let tabit = self.tabit.clone();
        match backend::spawn(cwd.as_deref(), tabit.as_deref(), resume, move || {
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
        self.display.confirm_new = false;
        self.start_backend(ctx, true);
    }

    /// Start a brand-new session: drop the backend (its stdin closes —
    /// the child aborts any in-flight run and winds down under the
    /// death contract), reset the transcript, and spawn without
    /// `--continue`. The old session's file stays on disk, untouched.
    fn new_session(&mut self, ctx: egui::Context) {
        self.backend = None;
        self.display.confirm_new = false;
        self.state = GuiState::default();
        self.start_backend(ctx, false);
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
        self.display.confirm_new = false;
        if let Some(backend) = &self.backend {
            backend.send_message(&text);
            self.state.message_sent(text);
        }
    }

    fn abort(&mut self) {
        self.display.confirm_new = false;
        if let Some(backend) = &self.backend
            && self.state.running
        {
            backend.abort();
        }
    }

    /// Answer one card — the send choke point for interactions (the
    /// `send` counterpart). The card closes optimistically; a stale id
    /// is a backend no-op.
    fn answer(&mut self, id: &str, option: Option<&str>, text: Option<&str>) {
        if self.state.phase != Phase::Live {
            return;
        }
        if let Some(backend) = &self.backend {
            backend.send_interaction_response(id, option, text);
            self.state.interaction_answered(id);
            self.display.answers.remove(id);
        }
    }

    /// The open-card panel: title, body, buttons, and (when invited)
    /// a free-text line. One block per card; several may be open at
    /// once, any answer order. Declared before the input panel, so it
    /// stacks directly above it.
    fn cards_panel(&mut self, ui: &mut egui::Ui) {
        let ids: Vec<String> = self
            .state
            .interactions
            .iter()
            .map(|c| c.id.clone())
            .collect();
        // Drafts for closed cards (answered or terminal-closed) go away.
        self.display.answers.retain(|id, _| ids.contains(id));
        if self.state.interactions.is_empty() {
            return;
        }
        egui::containers::Panel::bottom("cards").show(ui, |ui| {
            // Cloned for the widget pass: the send choke point needs
            // `&mut self` back (cards are small; this is the view).
            for card in self.state.interactions.clone() {
                ui.add_space(theme::ROW_GAP / 2.0);
                ui.horizontal(|ui| {
                    ui.add_space(theme::ROW_INSET);
                    ui.label(
                        egui::RichText::new(&card.title)
                            .strong()
                            .color(theme::ACCENT),
                    );
                });
                ui.horizontal(|ui| {
                    ui.add_space(theme::ROW_INSET);
                    ui.label(
                        egui::RichText::new(card.body.clone())
                            .monospace()
                            .color(theme::TEXT),
                    );
                });
                ui.horizontal(|ui| {
                    ui.add_space(theme::ROW_INSET);
                    let mut sent: Option<Option<String>> = None;
                    for label in &card.options {
                        if ui.button(label.clone()).clicked() {
                            sent = Some(Some(label.clone()));
                        }
                    }
                    if card.free_text {
                        let draft = self.display.answers.entry(card.id.clone()).or_default();
                        let field = ui.add(egui::TextEdit::singleline(draft).hint_text(
                            if card.options.is_empty() {
                                "your answer"
                            } else {
                                "optional note (goes to the model)"
                            },
                        ));
                        if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            sent = Some(None);
                        }
                        if ui.button("send").clicked() {
                            sent = Some(None);
                        }
                    }
                    if let Some(choice) = sent {
                        let text = self
                            .display
                            .answers
                            .get(&card.id)
                            .map(|d| d.trim().to_string())
                            .filter(|t| !t.is_empty());
                        self.answer(&card.id, choice.as_deref(), text.as_deref());
                    }
                });
            }
        });
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
        // A different session means a different transcript at the same
        // indexes (a respawn's replay, or the new-session action): the
        // height cache belongs to the old one.
        let session = self
            .state
            .facts
            .as_ref()
            .map(|facts| facts.session_id.clone());
        if session != self.display.session {
            self.display.session = session;
            self.display.heights.clear();
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
                        // Two-click new session: the first click arms,
                        // the second confirms — one accidental click
                        // must not drop a live conversation. The old
                        // session's file stays on disk, untouched.
                        let (label, enabled) = if self.display.confirm_new {
                            ("start new session?", true)
                        } else {
                            ("new session", !self.state.running)
                        };
                        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                            if self.display.confirm_new {
                                self.new_session(ctx.clone());
                            } else {
                                self.display.confirm_new = true;
                            }
                        }
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

        // 3. Open interaction cards (above the input panel), then the
        // input itself.
        self.cards_panel(ui);
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
        // `stick_to_bottom` owns tail-following (follows growth while
        // at the bottom, pauses when the user scrolls up, re-pins on
        // return); the height cache keeps scroll geometry stable —
        // off-screen rows are spaced by their last measured height, so
        // materializing rows don't shift the content under the user.
        egui::CentralPanel::default().show(ui, |ui| {
            let line_h = ui.text_style_height(&egui::TextStyle::Body);
            let width = ui.available_width();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show_viewport(ui, |ui, viewport| {
                    let mut y = 0.0;
                    for (index, group) in self.state.transcript.iter().enumerate() {
                        if self.display.heights.len() <= index {
                            self.display.heights.resize(index + 1, None);
                        }
                        // Sanctioned indexing (AGENTS.md doctrine): the
                        // resize above guarantees the slot exists.
                        #[allow(clippy::indexing_slicing)]
                        let height = self.display.heights[index]
                            .unwrap_or_else(|| estimate_height(group, width, line_h));
                        let visible = y + height + theme::ROW_GAP >= viewport.top() - 500.0
                            && y <= viewport.bottom() + 500.0;
                        if visible {
                            ui.add_space(theme::ROW_GAP);
                            let top = ui.cursor().top();
                            render_group(ui, group);
                            let rendered = ui.cursor().top() - top;
                            #[allow(clippy::indexing_slicing)]
                            {
                                self.display.heights[index] = Some(rendered);
                            }
                        } else {
                            ui.add_space(height + theme::ROW_GAP);
                        }
                        y += height + theme::ROW_GAP;
                    }
                    for pending in &self.state.pending {
                        ui.add_space(theme::ROW_GAP);
                        ui.label(
                            egui::RichText::new(format!("queued: {}", pending.text))
                                .color(theme::MUTED)
                                .italics(),
                        );
                    }
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
                            let mark = match (&tool.result, tool.done) {
                                (Some(result), _) if result.failed => "✗",
                                (Some(_), _) => "✓",
                                (None, _) => "…",
                            };
                            ui.label(
                                egui::RichText::new(format!("{mark} {}", tool.name))
                                    .color(theme::MUTED),
                            );
                        });
                        // The faithful copy: exactly what the model saw,
                        // failure detail included.
                        if let Some(result) = &tool.result {
                            ui.horizontal(|ui| {
                                ui.add_space(theme::ROW_INSET * 2.0);
                                ui.label(
                                    egui::RichText::new(result.content.clone())
                                        .color(theme::MUTED)
                                        .small(),
                                );
                            });
                        }
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
