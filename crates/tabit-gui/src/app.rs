//! The eframe app: display state + view. Thin by contract — every
//! decision lives in the reducer; this file only projects
//! [`GuiState`] to widgets and routes input back out.

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;

use crate::backend::{self, Backend};
use crate::reducer::{Group, GuiState, InMsg, Phase, Segment, SessionRow};
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
    /// The stream the height cache belongs to; switching views (a
    /// different transcript at the same indexes) invalidates it.
    stream: String,
    /// Crash-report toggle.
    show_stderr: bool,
    /// The model-switch test field (`provider/model` free text; the
    /// real picker belongs to the redesign — it needs a models-list
    /// command first).
    model_input: String,
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
        app.start_backend(ctx);
        app
    }

    /// Spawn the backend, booting the project's newest session. A
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
    /// the user's click is the rate limiter. Creating and switching
    /// sessions are commands, never respawns (v3).
    fn restart(&mut self, ctx: egui::Context) {
        self.backend = None;
        self.state = GuiState::default();
        self.start_backend(ctx);
    }

    /// Create a brand-new session in the backend — a command; the old
    /// sessions stay open behind it (switch back any time). The
    /// `session_created` event switches the view.
    fn new_session(&mut self) {
        if let Some(backend) = &self.backend {
            backend.new_session();
        }
    }

    /// Switch the active view to a known session — optimistic clear,
    /// then the backend's replay pass rebuilds the transcript.
    /// Re-opening the active session is a refresh: the same clear +
    /// replay path (idempotent by protocol design).
    fn open_session(&mut self, id: String) {
        self.state.open_session(&id);
        if let Some(backend) = &self.backend {
            backend.open_session(&id);
        }
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
        if let Some(backend) = &self.backend {
            let session = self.state.active.clone();
            backend.send_message(&session, &text);
            self.state.message_sent(text);
        }
    }

    fn abort(&mut self) {
        if let Some(backend) = &self.backend
            && self.state.running
        {
            let session = self.state.active.clone();
            backend.abort(&session);
        }
    }

    /// Checkout the active session at a transcript row's entry — the
    /// interim branch/rewind affordance (polish is a non-goal): "cut
    /// here" on a user message. Always sent over a Live channel; the
    /// backend parks it if a run is live, so there is no local gating
    /// to get wrong (FRONTEND.md §7).
    fn checkout(&mut self, entry_id: String) {
        if self.state.phase != Phase::Live {
            return;
        }
        if let Some(backend) = &self.backend {
            let session = self.state.active.clone();
            backend.checkout(&session, &entry_id);
        }
    }

    /// Switch the active session's model — the minimal test surface for
    /// the model command: `provider/model` free text, validated by the
    /// backend at receive (a bad ref is an error notice, nothing
    /// moves). No local gating: the backend parks a mid-run switch and
    /// lands it after the run, and the run itself is untouched.
    fn switch_model(&mut self) {
        if self.state.phase != Phase::Live {
            return;
        }
        let raw = self.display.model_input.trim().to_string();
        let Some((provider, model)) = raw.split_once('/') else {
            return;
        };
        self.display.model_input.clear();
        if let Some(backend) = &self.backend {
            backend.model(&self.state.active, provider, model);
        }
    }

    /// The answer payload for a card, by its template.
    #[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization (AGENTS.md doctrine)
    fn answer_payload(
        &self,
        card: &crate::reducer::InteractionCard,
        option: Option<&str>,
        text: Option<&str>,
    ) -> serde_json::Value {
        let clean = |value: Option<&str>, trim: bool| {
            value
                .map(|v| if trim { v.trim() } else { v })
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        match card {
            crate::reducer::InteractionCard::Confirm { .. } => {
                serde_json::to_value(tabit_protocol::templates::ConfirmAnswer {
                    option: clean(option, false),
                    text: clean(text, true),
                })
                .expect("template payloads always serialize")
            }
            crate::reducer::InteractionCard::Ask { .. } => {
                serde_json::to_value(tabit_protocol::templates::AskAnswer {
                    text: clean(text, true),
                })
                .expect("template payloads always serialize")
            }
        }
    }

    /// Answer one card — the send choke point for interactions (the
    /// `send` counterpart). The card closes optimistically; a stale id
    /// is a backend no-op.
    fn answer(&mut self, id: &str, option: Option<&str>, text: Option<&str>) {
        if self.state.phase != Phase::Live {
            return;
        }
        // Answers route by the card's own session — cards can belong
        // to a background stream (visible here only while viewing it,
        // but the routing must not assume the active stream).
        let Some(card) = self.state.interactions.iter().find(|c| c.id() == id) else {
            return;
        };
        let session = card.session().to_string();
        let payload = self.answer_payload(card, option, text);
        if let Some(backend) = &self.backend {
            backend.send_interaction_response(&session, id, &payload);
            self.state.interaction_answered(id);
            self.display.answers.remove(id);
        }
    }

    /// The open-card panel: title, body, buttons, and (when invited)
    /// a free-text line. One block per card; several may be open at
    /// once, any answer order. Declared before the input panel, so it
    /// stacks directly above it.
    fn cards_panel(&mut self, ui: &mut egui::Ui) {
        // Cards are per-session; the panel renders the active
        // session's (background cards wait on their switcher rows).
        let active = self.state.active.clone();
        let cards: Vec<crate::reducer::InteractionCard> = self
            .state
            .interactions
            .iter()
            .filter(|c| c.session() == active)
            .cloned()
            .collect();
        let ids: Vec<String> = cards.iter().map(|c| c.id().to_string()).collect();
        // Drafts for closed cards (answered or terminal-closed) go away.
        self.display.answers.retain(|id, _| ids.contains(id));
        if cards.is_empty() {
            return;
        }
        egui::containers::Panel::bottom("cards").show(ui, |ui| {
            // Cloned for the widget pass: the send choke point needs
            // `&mut self` back (cards are small; this is the view).
            for card in cards {
                ui.add_space(theme::ROW_GAP / 2.0);
                // The template decides the shape; each variant renders
                // its own heading, content, and answer affordances.
                let (id, heading, content, options, free_text) = match &card {
                    crate::reducer::InteractionCard::Confirm {
                        id,
                        title,
                        body,
                        options,
                        free_text,
                        ..
                    } => (
                        id.clone(),
                        title.clone(),
                        Some(body.clone()),
                        options.clone(),
                        *free_text,
                    ),
                    crate::reducer::InteractionCard::Ask { id, prompt, .. } => (
                        id.clone(),
                        "Question from the assistant".to_string(),
                        Some(prompt.clone()),
                        Vec::new(),
                        true,
                    ),
                };
                ui.horizontal(|ui| {
                    ui.add_space(theme::ROW_INSET);
                    ui.label(egui::RichText::new(heading).strong().color(theme::ACCENT));
                });
                if let Some(content) = content {
                    ui.horizontal(|ui| {
                        ui.add_space(theme::ROW_INSET);
                        ui.label(egui::RichText::new(content).monospace().color(theme::TEXT));
                    });
                }
                ui.horizontal(|ui| {
                    ui.add_space(theme::ROW_INSET);
                    let mut sent: Option<Option<String>> = None;
                    for label in &options {
                        if ui.button(label.clone()).clicked() {
                            sent = Some(Some(label.clone()));
                        }
                    }
                    if free_text {
                        let draft = self.display.answers.entry(id.clone()).or_default();
                        let field = ui.add(egui::TextEdit::singleline(draft).hint_text(
                            if options.is_empty() {
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
                            .get(&id)
                            .map(|d| d.trim().to_string())
                            .filter(|t| !t.is_empty());
                        self.answer(&id, choice.as_deref(), text.as_deref());
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
        // A different active stream means a different transcript at
        // the same indexes (a switch's replay, or a new session): the
        // height cache belongs to the old one.
        if self.state.active != self.display.stream {
            self.display.stream = self.state.active.clone();
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
        // Only materialized once the backend is gone (the tail is a
        // mutex-locked 200-line clone; Live frames never read it).
        let stderr_tail = if matches!(phase, Phase::Exited { .. }) {
            self.backend
                .as_ref()
                .map(|b| b.stderr_tail())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
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
                    // The session switcher: every known session (the
                    // startup catalog plus anything created over this
                    // connection), liveness dots included. Selecting a
                    // row switches optimistically; the replay pass
                    // rebuilds the view. "new session" is a command —
                    // the old sessions stay open behind it.
                    let selected = self
                        .state
                        .sessions
                        .iter()
                        .find(|row| row.id == self.state.active);
                    let selected_label =
                        selected.map_or_else(|| "session".to_string(), session_label);
                    let mut picked = None;
                    egui::ComboBox::from_id_salt("sessions")
                        .selected_text(selected_label)
                        .width(230.0)
                        .show_ui(ui, |ui| {
                            for row in &self.state.sessions {
                                let label = if row.id == self.state.active {
                                    format!("{} (this one)", session_label(row))
                                } else {
                                    session_label(row)
                                };
                                if ui.selectable_label(false, label).clicked() {
                                    picked = Some(row.id.clone());
                                }
                            }
                        });
                    if let Some(id) = picked {
                        self.open_session(id);
                    }
                    if ui.button("new session").clicked() {
                        self.new_session();
                    }
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
                    // The model switch, minimal test surface: type
                    // `provider/model`, Enter or the button applies. The
                    // label above stays truth until `model_changed`
                    // lands (the backend validates at receive).
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.display.model_input)
                            .desired_width(110.0)
                            .hint_text("provider/model"),
                    );
                    let mut apply =
                        field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("set model").clicked() {
                        apply = true;
                    }
                    if apply {
                        self.switch_model();
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
            // A row's rewind click, collected during the render pass and
            // acted on after it (the closure cannot borrow `self` for
            // the send).
            let mut checkout_target: Option<String> = None;
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
                            if let Some(entry_id) = render_group(ui, group) {
                                checkout_target = Some(entry_id);
                            }
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
            if let Some(entry_id) = checkout_target {
                self.checkout(entry_id);
            }
        });

        // 5. Keep frames coming while anything is in flight.
        if self.state.running || self.state.phase == Phase::Connecting {
            self_repaint(&ctx);
        }
    }
}

/// One switcher row's label. Session ids are UUIDv7 — time-ordered —
/// so the id's HEAD is shared by everything created in the same
/// weeks-long window and distinguishes nothing; show the creation
/// time (catalog rows) and the id's random TAIL instead.
fn session_label(row: &SessionRow) -> String {
    let tail = row
        .id
        .get(row.id.len().saturating_sub(6)..)
        .map(str::to_string)
        .unwrap_or_else(|| row.id.clone());
    let created = if row.created_at.is_empty() {
        "new".to_string()
    } else {
        // "2026-08-22T14:32:05Z" → "08-22 14:32" — the date's year and
        // the seconds earn nothing in a switcher.
        row.created_at
            .get(5..16)
            .map(|slice| slice.replacen('T', " ", 1))
            .unwrap_or_else(|| row.created_at.clone())
    };
    let mut label = format!("{created} …{tail} · {} entries", row.entry_count);
    if row.running {
        label.push_str(" ●");
    }
    if row.attention {
        label.push_str(" !");
    }
    label
}

/// Estimated rendered height of one group — the virtualization
/// approximation; rows near the viewport render and self-correct.
fn estimate_height(group: &Group, width: f32, line_h: f32) -> f32 {
    let text_lines = |text: &str| -> f32 {
        let chars_per_line = ((width - 2.0 * theme::ROW_INSET) / (line_h * 0.55)).max(1.0) as usize;
        text.len().div_ceil(chars_per_line).max(1) as f32
    };
    match group {
        Group::User { text, .. } => text_lines(text) * line_h,
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

/// Render one transcript group. `Some(entry_id)` means the row's
/// rewind button was clicked — the caller sends the checkout.
fn render_group(ui: &mut egui::Ui, group: &Group) -> Option<String> {
    match group {
        Group::User { text, entry_id } => {
            let mut checkout = None;
            ui.horizontal(|ui| {
                ui.add_space(theme::ROW_INSET);
                ui.label(egui::RichText::new(format!("you: {text}")).color(theme::USER_TEXT));
                // The interim checkout affordance: "cut here" (the
                // chain ends at this message; the next prompt branches
                // from it). Always enabled — the backend parks a
                // mid-run checkout at the pause point.
                if ui
                    .button(egui::RichText::new("⟲").small())
                    .on_hover_text("checkout here — rewind the session to this message")
                    .clicked()
                {
                    checkout = Some(entry_id.clone());
                }
            });
            checkout
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
            None
        }
        Group::Notice { text, error } => {
            let color = if *error { theme::ERROR } else { theme::MUTED };
            ui.horizontal(|ui| {
                ui.add_space(theme::ROW_INSET);
                ui.label(egui::RichText::new(text.clone()).color(color));
            });
            None
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
            None
        }
    }
}
