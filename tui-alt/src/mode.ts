/**
 * The interactive mode: a typed handler table over the backend's frames —
 * one handler per wire event, and the same table serves live traffic and
 * the replay pass (a pass arrives as the same vocabulary with live ids,
 * FRONTEND.md §7 — one transcript-rebuild path, no mode-side branching).
 *
 * Ownership follows components-as-state (TUI-RESEARCH §9): the mode keeps
 * only cross-cutting session state — active session, running, pending
 * queue, open cards, usage totals — while per-block content lives in the
 * view (the alt-screen root's engine components, or the test recorder).
 * The seam is `ModeView`; every method is synchronous.
 *
 * Transcript structure rule: **blocks are created on demand by their first
 * content** — `turn_started` allocates nothing, so a turn's text, thinking,
 * and tool rows stack in true wire-arrival order. Deltas coalesce into one
 * ordered buffer (text and reasoning interleaved as they arrived), flushed
 * every ~33 ms; within a flush batch the increments apply in wire order.
 *
 * Stream routing is announcement-driven (SUBAGENTS.md): frames whose stamp
 * is not the active session are child traffic — logged, not rendered until
 * focus switching lands (M2). Nothing gates on prior knowledge of a stream.
 * The report model (v19): the backend speaks first (`report`), there is no
 * handshake ack, and the routing key arrives ON the boot's stamped
 * `session_opened` — so the foreign-stamp drop stands down until that
 * announcement lands (before it, the boot's stream is the only one that
 * exists).
 */

import { log } from "./log.ts";
import { SessionTree } from "./session-tree.ts";
import { PROTOCOL_VERSION } from "./protocol.ts";
import type { AvailableProvider, ModelCost, ParsedServerFrame, ProviderStatus, ServerControlFrame, SessionEvent, Usage } from "./protocol.ts";

export interface PendingMessage {
	id: string;
	text: string;
}

export interface InteractionCard {
	id: string;
	uiType: string;
	title: string;
	body: string;
	/** Option labels in wire order; the answer indexes into these. */
	options: string[];
	freeText: boolean;
}

export interface FooterFacts {
	session: string | undefined;
	/** The session log file's path (from `session_opened`) — the future
	 *  session UI's open target; undefined for ephemeral sessions. */
	path: string | undefined;
	/** The session's working directory (v16) — the completion root. A
	 *  child's spawn cwd differs from the frontend's, so this comes from
	 *  the wire, never from process.cwd(). */
	cwd: string | undefined;
	model: string | undefined;
	/** Config's display name for the model (v11) — absent means unstated. */
	modelName: string | undefined;
	/** Context meter denominator in tokens (v11) — absent means unstated. */
	contextWindow: number | undefined;
	resumed: boolean;
	/** Sums over `completion_call`s (v12: per-turn is the home, sums are
	 *  ours) — rebuilt by replay passes, so they survive resume. The
	 *  cache-write sum rides along as data (usage is usage) but no badge
	 *  renders it — the usage display ignores cache writes (owner ruling). */
	inputTokens: number;
	outputTokens: number;
	cachedInputTokens: number;
	cacheCreationTokens: number;
	/** The latest request's cache hit rate (cacheRead over all prompt
	 *  legs, percent) — a latest-wins fact, not a sum. */
	cacheHitRate: number | undefined;
	/** Recorded dollars (v13), summed over costed turns — absent until the
	 *  first costed turn, never fabricated as zero. */
	cost: number | undefined;
	/** Current context length in tokens — the last completion_call's total,
	 *  or `compaction_end`'s `tokens_after` (v15), whichever arrived last.
	 *  Absent before the first report. The context badge divides by
	 *  `contextWindow`. */
	contextUsed: number | undefined;
	/** The active model's rate card (v11, USD per million tokens) — what
	 *  future turns will cost, not what they did. */
	rates: ModelCost | undefined;
	running: boolean;
}

export interface SkillInfo {
	name: string;
	description: string;
}

/** One resolved app keybinding, for `/help` display. */
export interface KeybindingFact {
	action: string;
	keys: string[];
	description: string;
}

/**
 * The rendering seam: implemented by the alt-screen root with live engine
 * components, and by tests as a recorder. The mode never holds rendered
 * text beyond what the ordered delta buffer carries.
 */
export interface ModeView {
	beginReplay(): void;
	endReplay(): void;
	/** The skill catalog (from `skills_available`) — the editor's `/`
	 *  completion lists them; invocation is display-only (no wire command). */
	setSkills(skills: SkillInfo[]): void;
	addUser(entryId: string, text: string): void;
	addNote(text: string, kind: "info" | "warn" | "error"): void;
	/** Lazy: creates the turn's assistant block on the first increment. */
	appendAssistantText(turnId: string, text: string): void;
	/** Lazy: creates the thinking block on the first increment; the view
	 *  accumulates per `reasoningId` (same id = same block, FRONTEND.md §5). */
	appendReasoning(turnId: string, reasoningId: string, text: string): void;
	addTool(turnId: string, internalCallId: string, name: string, args: string | null): void;
	setToolResult(internalCallId: string, content: string, ok: boolean, details?: unknown): void;
	/** Drop every provisional block the turn created (`turn_retried`). */
	removeTurn(turnId: string): void;
	setPending(pending: PendingMessage[]): void;
	setStatus(text: string): void;
	setFooter(facts: FooterFacts): void;
	showCard(card: InteractionCard): void;
	closeCard(id: string, note: string | undefined): void;
}

/** What the mode needs of the backend — `Backend` satisfies it structurally. */
export interface BackendLink {
	message(session: string, text: string): void;
	abort(session: string): void;
	/** Manual compaction; `directives` is the user's free-text guidance
	 *  for this invocation (v16 — appended to the instruction, never
	 *  persisted). */
	compact(session: string, directives?: string): void;
	/** Rewind the active chain to an entry (any entry the wire ever named —
	 *  an off-chain target is a branch switch). The backend composes abort;
	 *  the outcome is `checked_out` + a full replay pass. */
	checkout(session: string, entryId: string): void;
	/** Switch the session's model (the `/model` picker's select).
	 *  `model_changed` lands the resolved facts. */
	setModel(session: string, provider: string, model: string): void;
	interactionResponse(session: string, id: string, payload: unknown): void;
	/** Store a provider key (v21) — the re-announced catalog is the ack. */
	login(provider: string, apiKey: string): void;
	/** Remove a provider's key (v21) — total, idempotent, still acked. */
	logout(provider: string): void;
}

const FLUSH_MS = 33;

type SessionEventType = SessionEvent["type"];
type Handler<K extends SessionEventType> = (event: Extract<SessionEvent, { type: K }>) => void;

/** One coalesced delta, kept in arrival order so a flush replays the wire. */
type PendingDelta =
	| { kind: "text"; turnId: string; text: string }
	| { kind: "reasoning"; turnId: string; id: string; text: string };

export class InteractiveMode {
	readonly #backend: BackendLink;
	readonly #view: ModeView;
	#session: string | undefined;
	#running = false;
	#replaying = false;
	#provider: string | undefined;
	#model: string | undefined;
	#modelName: string | undefined;
	#contextWindow: number | undefined;
	#path: string | undefined;
	#cwd: string | undefined;
	#resumed = false;
	#inputTokens = 0;
	#outputTokens = 0;
	#cachedInputTokens = 0;
	#cacheCreationTokens = 0;
	#cacheHitRate: number | undefined;
	#cost: number | undefined;
	#contextUsed: number | undefined;
	#rates: ModelCost | undefined;
	/** The usable-model catalog (v21) — backend-level, folded last-wins
	 *  on every `models_available` (the boot announcement and each world
	 *  refresh). The picker's source when it lands. */
	#catalog: AvailableProvider[] = [];
	/** Every configured provider with its winning key source (v22) —
	 *  unconditional, folded last-wins with each world refresh. Login
	 *  targets re-derive from it (`auth: "none"`); `env` is display-only. */
	#providerStatuses: ProviderStatus[] = [];
	#pending: PendingMessage[] = [];
	#skills: SkillInfo[] = [];
	#keybindings: KeybindingFact[] = [];
	/** The client-built session tree (see session-tree.ts) — fed by the
	 *  same handler table, so replay passes resync it for free. */
	readonly #tree = new SessionTree();
	readonly #cards = new Map<string, InteractionCard>();
	readonly #deltas: PendingDelta[] = [];
	#flushTimer: ReturnType<typeof setTimeout> | undefined;
	/** Set for fatal backend reports (version mismatch, startup failure);
	 *  the entry turns it into the exit path. */
	onFatal: ((reason: string) => void) | undefined;
	/** Set by the entry: the graceful shutdown path (`/exit`, `/quit`). */
	onQuit: (() => void) | undefined;
	/** Set by the root: opens the model picker (`/model`). */
	onModel: (() => void) | undefined;
	/** Set by the root: opens the login card (`/login`). */
	onLogin: (() => void) | undefined;
	/** Set by the root: opens the logout card (`/logout`). */
	onLogout: (() => void) | undefined;
	/** Set by the root: opens the session-tree card (`/tree`; the ctrl+t
	 *  action routes through the root too). */
	onTree: (() => void) | undefined;

	constructor(backend: BackendLink, view: ModeView) {
		this.#backend = backend;
		this.#view = view;
		this.#view.setStatus("connecting…");
	}

	get activeSession(): string | undefined {
		return this.#session;
	}

	/** Resolved app keybindings, for `/help`. Called by the entry after
	 *  the registry is installed. */
	setKeybindings(facts: KeybindingFact[]): void {
		this.#keybindings = facts;
	}

	get running(): boolean {
		return this.#running;
	}

	/** The usable-model catalog, latest announcement (v21). */
	get modelsCatalog(): readonly AvailableProvider[] {
		return this.#catalog;
	}

	/** Every configured provider with its winning key source (v22). */
	get providerStatuses(): readonly ProviderStatus[] {
		return this.#providerStatuses;
	}

	/** The session's register (provider + model ids), for the picker's
	 *  current marker. Undefined when the session has no selection (v21's
	 *  zero-config boot). */
	get currentSelection(): { provider: string; model: string } | undefined {
		return this.#provider === undefined || this.#model === undefined ? undefined : { provider: this.#provider, model: this.#model };
	}

	/** The session tree — read-only for the view; the mode feeds it. */
	get tree(): SessionTree {
		return this.#tree;
	}

	/** Rewind to an entry (the tree card's enter). Fire-and-forget like
	 *  every command; `checked_out` + the replay pass are the outcome. */
	checkout(entryId: string): void {
		if (this.#session) this.#backend.checkout(this.#session, entryId);
	}

	get replaying(): boolean {
		return this.#replaying;
	}

	/** Editor submit: slash space first (`/compact [guidance]` is a wire
	 *  command — the guidance rides the frame as this invocation's
	 *  directives; `/help` lists keys and commands; `exit`/`quit` end the
	 *  TUI; skills are display-only — no wire invocation exists, so
	 *  selecting one warns instead of sending), else a plain message. */
	submit(text: string): void {
		if (!this.#session || text === "") return;
		if (text.startsWith("/")) {
			const body = text.slice(1).trim();
			const space = body.indexOf(" ");
			const name = space === -1 ? body : body.slice(0, space);
			const args = space === -1 ? "" : body.slice(space + 1).trim();
			const entry = this.#slashEntries().find(candidate => candidate.name === name);
			if (entry?.run !== undefined) entry.run(args);
			else this.#view.addNote(`/${name} is not invocable yet — listed for discovery only`, "warn");
			return;
		}
		this.#backend.message(this.#session, text);
	}

	/**
	 * The slash command set — the one home. The dropdown reads it and the
	 * interpreter runs it, so a command cannot exist in one and not the
	 * other (the /help-shipped-but-unlisted miss was exactly that split).
	 * An entry without `run` is display-only: listed, warned on select.
	 */
	slashCommands(): Array<{ name: string; description: string; displayOnly: boolean }> {
		return this.#slashEntries().map(({ name, description, run }) => ({ name, description, displayOnly: run === undefined }));
	}

	#slashEntries(): Array<{ name: string; description: string; run?: (args: string) => void }> {
		return [
			{
				name: "compact",
				description: "summarize the context now",
				run: args => {
					const directives = args === "" ? undefined : args;
					this.#backend.compact(this.#session!, directives);
				},
			},
			{ name: "help", description: "list keys and commands", run: () => this.#showHelp() },
			{ name: "login", description: "store a provider API key", run: () => this.onLogin?.() },
			{ name: "logout", description: "remove a stored provider key", run: () => this.onLogout?.() },
			{ name: "model", description: "switch the model", run: () => this.onModel?.() },
			{ name: "tree", description: "browse the session tree, rewind to an entry", run: () => this.onTree?.() },
			{ name: "exit", description: "quit the TUI (shuts the backend down)", run: () => this.onQuit?.() },
			{ name: "quit", description: "quit the TUI (shuts the backend down)", run: () => this.onQuit?.() },
			...this.#skills.map(skill => ({ name: skill.name, description: skill.description })),
		];
	}

	#showHelp(): void {
		const names = this.#slashEntries().map(entry => `/${entry.name}`).join(" ·  ");
		this.#view.addNote(`commands ·  ${names} — @ paths complete files`, "info");
		for (const fact of this.#keybindings) {
			this.#view.addNote(`keys ·  ${fact.description}: ${fact.keys.join(" / ")}`, "info");
		}
	}

	/** The picker's select: fire the `model` command at the active session
	 *  (a state write at receive; `model_changed` answers). */
	switchModel(provider: string, model: string): void {
		if (this.#session) this.#backend.setModel(this.#session, provider, model);
	}

	/** The login card's confirm: store the key. Backend-level and
	 *  session-less (v21) — the re-announced catalogs are the ack. */
	login(provider: string, apiKey: string): void {
		this.#backend.login(provider, apiKey);
	}

	/** The logout card's pick: remove the stored key (total, idempotent). */
	logout(provider: string): void {
		this.#backend.logout(provider);
	}

	interrupt(): void {
		if (this.#session && this.#running) this.#backend.abort(this.#session);
	}

	/** Answer a card: `selected` echoes the chosen labels (exactly one
	 *  for select_one, 0..n for select_any), `text` carries the note when
	 *  the card invited one. */
	answerCard(id: string, selected: string[], text: string | null): void {
		const card = this.#cards.get(id);
		if (!card) return;
		this.#cards.delete(id);
		this.#backend.interactionResponse(this.#session!, id, { selected, text });
		this.#view.closeCard(id, undefined);
	}

	/** Whether an interaction card is open — the input listener stands
	 *  down (bar Ctrl+C) while it is, so the card owns the keyboard. */
	get hasOpenCard(): boolean {
		return this.#cards.size > 0;
	}

	handleFrame(parsed: ParsedServerFrame): void {
		if (parsed.kind === "control") return this.#handleControl(parsed.frame);
		if (parsed.kind === "unknown") {
			log(`unknown frame${parsed.type ? ` type=${parsed.type}` : " (unparseable)"}: ${parsed.raw}`);
			this.#view.addNote(`unknown frame${parsed.type ? ` (${parsed.type})` : ""} — logged, connection kept`, "warn");
			return;
		}
		// A backend-level error before the boot session opened is the
		// startup-failure shape (v19): the report, this one unstamped error
		// carrying the reason (config problems carry the setup guide), then
		// a nonzero exit. Display the reason and die — respawn is the fix.
		if (parsed.stream === undefined && parsed.event.type === "error" && this.#session === undefined) {
			this.onFatal?.(parsed.event.message);
			return;
		}
		// Child traffic: the announcement (session_opened with a parent)
		// renders a note; everything else on a foreign stamp logs only. The
		// drop stands down until the boot's session_opened lands — the
		// routing key arrives on that stamped frame itself (v19), and
		// before it the boot's stream is the only one that exists.
		if (this.#session !== undefined && parsed.stream !== undefined && parsed.stream !== this.#session) {
			log(`child stream ${parsed.stream}${parsed.origin ? ` (origin ${parsed.origin})` : ""}: ${parsed.event.type}`);
			return;
		}
		const event = parsed.event;
		const handler = this.#handlers[event.type] as ((event: SessionEvent) => void) | undefined;
		handler?.(event);
	}

	#handleControl(frame: ServerControlFrame): void {
		if (frame.type === "report") {
			// The report model (v19): the backend speaks first and WE are the
			// version check — a report naming any other version means our event
			// vocabulary is wrong for this pipe. Fail loud, never limp on
			// unknown frames; the entry's exit kills the mismatched child.
			if (frame.protocol_version !== PROTOCOL_VERSION) {
				this.onFatal?.(
					`protocol version mismatch: backend speaks v${frame.protocol_version}, this frontend speaks v${PROTOCOL_VERSION}`,
				);
			}
			return;
		}
		this.#view.addNote(`protocol error: ${frame.message}`, "error");
	}

	#setRunning(running: boolean): void {
		if (this.#running === running) return;
		this.#running = running;
		this.#view.setStatus(running ? "working — esc interrupts" : "idle");
		this.#emitFooter();
	}

	#emitFooter(): void {
		this.#view.setFooter({
			session: this.#session,
			path: this.#path,
			cwd: this.#cwd,
			model: this.#model,
			modelName: this.#modelName,
			contextWindow: this.#contextWindow,
			resumed: this.#resumed,
			inputTokens: this.#inputTokens,
			outputTokens: this.#outputTokens,
			cachedInputTokens: this.#cachedInputTokens,
			cacheCreationTokens: this.#cacheCreationTokens,
			cacheHitRate: this.#cacheHitRate,
			cost: this.#cost,
			contextUsed: this.#contextUsed,
			rates: this.#rates,
			running: this.#running,
		});
	}

	/** The one usage fold: a completion_call and a compaction_step meter
	 *  identically (v15 — compaction spend is spend), so totals are one
	 *  fold over both event kinds. Also tracks the freshest context
	 *  length (`compaction_end` overwrites with its authoritative
	 *  `tokens_after`) and the latest request's cache hit rate. */
	#meter(usage: Usage, cost?: number): void {
		this.#inputTokens += usage.input_tokens;
		this.#outputTokens += usage.output_tokens;
		this.#cachedInputTokens += usage.cached_input_tokens;
		this.#cacheCreationTokens += usage.cache_creation_input_tokens;
		const promptLegs = usage.input_tokens + usage.cached_input_tokens + usage.cache_creation_input_tokens;
		this.#cacheHitRate = promptLegs > 0 ? (usage.cached_input_tokens / promptLegs) * 100 : undefined;
		if (cost !== undefined) this.#cost = (this.#cost ?? 0) + cost;
		this.#contextUsed = usage.total_tokens;
		this.#emitFooter();
	}

	#scheduleFlush(): void {
		this.#flushTimer ??= setTimeout(() => this.#flush(), FLUSH_MS);
	}

	#flush(): void {
		if (this.#flushTimer !== undefined) {
			clearTimeout(this.#flushTimer);
			this.#flushTimer = undefined;
		}
		for (const delta of this.#deltas) {
			if (delta.kind === "text") this.#view.appendAssistantText(delta.turnId, delta.text);
			else this.#view.appendReasoning(delta.turnId, delta.id, delta.text);
		}
		this.#deltas.length = 0;
	}

	#dropTurnDeltas(turnId: string): void {
		// A retried turn's still-buffered deltas must never paint: drop them
		// before anything flushes. Other turns' deltas stay buffered.
		for (let index = this.#deltas.length - 1; index >= 0; index--) {
			if (this.#deltas[index]!.turnId === turnId) this.#deltas.splice(index, 1);
		}
	}

	#closeAllCards(reason: string): void {
		for (const id of this.#cards.keys()) this.#view.closeCard(id, reason);
		this.#cards.clear();
	}

	readonly #handlers: { [K in SessionEventType]: Handler<K> } = {
		// --- run lifecycle ---------------------------------------------------
		user_message: event => {
			if (!this.#replaying) this.#setRunning(true);
			this.#tree.addUser(event.entry_id, event.text);
			this.#view.addUser(event.entry_id, event.text);
			const next = this.#pending.filter(p => p.id !== event.entry_id);
			if (next.length !== this.#pending.length) {
				this.#pending = next;
				this.#view.setPending(this.#pending);
			}
		},
		run_finished: event => {
			this.#flush();
			this.#setRunning(false);
			if (!event.durable) {
				this.#view.addNote("log writes are degraded — this run may not survive a crash", "warn");
			}
			this.#closeAllCards("run finished");
		},
		run_aborted: event => {
			this.#flush();
			this.#setRunning(false);
			this.#closeAllCards("run aborted");
			this.#view.addNote("run aborted", "warn");
			if (event.output !== "") log(`aborted with partial output (${event.output.length} chars)`);
		},
		run_failed: event => {
			this.#flush();
			this.#setRunning(false);
			this.#view.addNote(`run failed (${event.kind}): ${event.message}`, "error");
			// Pending messages survive a failure — they drain into the next run.
		},
		// --- queueing ------------------------------------------------------------
		message_queued: event => {
			this.#pending = [...this.#pending, { id: event.id, text: event.text }];
			this.#view.setPending(this.#pending);
		},
		messages_discarded: event => {
			this.#pending = [];
			this.#view.setPending(this.#pending);
			this.#view.addNote(`${event.messages.length} queued message(s) discarded — the backend kept no copy`, "warn");
		},
		// --- transcript ----------------------------------------------------------
		// turn_started allocates nothing: blocks are created on demand by
		// their first content, so document order == wire arrival order.
		turn_started: event => {
			this.#tree.openTurn(event.id);
		},
		text_delta: event => {
			this.#tree.appendTurnText(event.turn_id, event.text);
			this.#deltas.push({ kind: "text", turnId: event.turn_id, text: event.text });
			this.#scheduleFlush();
		},
		reasoning_delta: event => {
			this.#deltas.push({ kind: "reasoning", turnId: event.turn_id, id: event.id, text: event.reasoning });
			this.#scheduleFlush();
		},
		turn_committed: () => {},
		turn_retried: event => {
			this.#tree.retryTurn(event.turn_id);
			this.#dropTurnDeltas(event.turn_id);
			this.#view.removeTurn(event.turn_id);
			this.#view.addNote("turn discarded before commit — a retry follows", "info");
		},
		turn_truncated: () => {
			this.#view.addNote("turn hit the provider output limit — the run continues", "warn");
		},
		tool_call: event => {
			this.#tree.noteToolCall(event.internal_call_id, event.name, event.arguments);
			this.#view.addTool(event.turn_id, event.internal_call_id, event.name, event.arguments);
		},
		tool_result: event => {
			this.#tree.addTool(event.entry_id, event.internal_call_id, event.name, event.content);
			this.#view.setToolResult(event.internal_call_id, event.content, event.status.status === "success", event.details);
		},
		completion_call: event => {
			// v12: the per-turn report is the only home — every request's
			// usage counts, aborted and failed runs included, and replay
			// passes re-deliver it so sums survive resume.
			this.#meter(event.usage, event.cost);
		},
		native_item: () => {
			// Provider-native, live-only, never replayed — nothing to render yet.
			log("native_item received");
		},
		// --- errors / durability ---------------------------------------------------
		error: event => {
			if (event.kind === "persist_degraded") {
				this.#view.addNote(`log writes degraded — ${event.pending ?? "?"} record(s) pending flush`, "warn");
				return;
			}
			if (event.kind === "persist_recovered") {
				this.#view.addNote("log writes recovered", "info");
				return;
			}
			// kind "model" is a degradation, not a death (§6): a fallback
			// named, or the zero-config teaching note — the session runs on.
			// kind "auth" (v21) is a failed login/logout — an error note.
			this.#view.addNote(`error (${event.kind}): ${event.message}`, event.kind === "model" ? "warn" : "error");
		},
		// --- replay ------------------------------------------------------------------
		replay_begin: () => {
			this.#flush();
			this.#replaying = true;
			this.#deltas.length = 0;
			this.#tree.closeTurn();
			this.#view.beginReplay();
		},
		replay_end: () => {
			this.#replaying = false;
			this.#view.endReplay();
		},
		checked_out: event => {
			// The head moves now; the full re-render arrives as the following
			// replay brackets (which also re-walk the tree's shared prefix).
			this.#tree.checkout(event.entry_id);
			this.#view.addNote(`rewound to ${event.entry_id}`, "info");
		},
		// --- announcements / config ----------------------------------------------------
		sessions_available: event => {
			this.#view.addNote(`${event.sessions.length} session(s) on disk`, "info");
		},
		skills_available: event => {
			// v20: stamped with the session's stream, announced as each session
			// becomes visible. The foreign-stamp drop keeps a child's catalog
			// off this list; session switches clear in session_opened.
			this.#skills = event.skills.map(skill => ({ name: skill.name, description: skill.description ?? "" }));
			this.#view.setSkills(this.#skills);
			this.#view.addNote(`${event.skills.length} skill(s) loaded: ${event.skills.map(s => s.name).join(", ")}`, "info");
		},
		extensions_available: event => {
			for (const ext of event.extensions) {
				if (ext.status === "dead") this.#view.addNote(`extension ${ext.name} is dead: ${ext.reason ?? "unknown reason"}`, "error");
			}
			for (const conflict of event.conflicts) {
				this.#view.addNote(
					`extension conflict (${conflict.kind}): ${conflict.extension}'s tool "${conflict.tool}"${conflict.incumbent ? ` displaces ${conflict.incumbent}` : ""}`,
					"warn",
				);
			}
			this.#view.addNote(`${event.extensions.length} extension(s) loaded`, "info");
		},
		session_opened: event => {
			if (event.parent !== undefined) {
				this.#view.addNote("subagent session started — focus switching lands in M2", "info");
				return;
			}
			this.#session = event.id;
			// v21: null = no selection (the zero-config boot) — footer facts
			// stay undefined until the first model command lands one.
			this.#provider = event.model?.provider;
			this.#model = event.model?.model;
			// Empty path = ephemeral session (nothing on disk to open).
			this.#path = event.path === "" ? undefined : event.path;
			this.#cwd = event.cwd === "" ? undefined : event.cwd;
			this.#resumed = event.resumed;
			// Per-session facts reset here: the model_changed ahead of the
			// following replay restates the resolved record, and the pass
			// re-delivers completion_calls, so sums rebuild from history.
			this.#modelName = undefined;
			this.#contextWindow = undefined;
			this.#rates = undefined;
			this.#inputTokens = 0;
			this.#outputTokens = 0;
			this.#cachedInputTokens = 0;
			this.#cacheCreationTokens = 0;
			this.#cacheHitRate = undefined;
			this.#cost = undefined;
			this.#contextUsed = undefined;
			// v20: skills are per-session — announced only when discovery found
			// at least one, so absence is unambiguous and the old session's list
			// must not survive into this one.
			this.#skills = [];
			this.#view.setSkills(this.#skills);
			this.#tree.reset();
			this.#emitFooter();
			// Boot facts have landed — the connection is no longer "connecting".
			if (!this.#running) this.#view.setStatus("idle");
		},
		model_changed: event => {
			this.#provider = event.provider;
			this.#model = event.model;
			this.#modelName = event.name;
			this.#contextWindow = event.context_window;
			this.#rates = event.cost;
			this.#emitFooter();
		},
		models_available: event => {
			// v21: the usable-model picker catalog — backend-level, always
			// emitted (even empty), re-announced on world change, last-wins.
			// The setup-state warning rides providers_available (v22), which
			// follows in the same act with the full key-source picture.
			this.#catalog = event.providers;
			if (event.providers.length === 0) return;
			const models = event.providers.reduce((n, p) => n + p.models.length, 0);
			this.#view.addNote(`${models} model(s) across ${event.providers.length} provider(s)`, "info");
		},
		providers_available: event => {
			// v22: unconditional, one act with models_available. The setup
			// predicate (§3.2) branches the first-run teaching here.
			this.#providerStatuses = event.providers;
			if (event.providers.length === 0) {
				this.#view.addNote("no providers configured at this backend — write ~/.tabit/providers.toml and restart", "warn");
				return;
			}
			const missing = event.providers.filter(p => p.auth === "none");
			if (this.#catalog.length === 0) {
				this.#view.addNote(`no usable models — missing keys for: ${missing.map(p => p.id).join(", ")}`, "warn");
				return;
			}
			if (missing.length > 0) {
				this.#view.addNote(`no key for: ${missing.map(p => p.id).join(", ")} — login fixes in-app`, "info");
			}
		},
		// --- interactions ------------------------------------------------------------------
		interaction_request: event => {
			const card = parseCard(event.id, event.ui_type, event.payload);
			if (card === undefined) {
				// Unknown ui_type or malformed payload: surface it, never
				// fabricate an answer (FRONTEND.md §8). The run stays blocked
				// until its terminal closes the card.
				this.#view.addNote(`cannot answer card (${event.ui_type}) — unsupported shape`, "warn");
				return;
			}
			this.#cards.set(card.id, card);
			this.#view.showCard(card);
		},
		interaction_settled: event => {
			// v17: the settle close — the request was answered, retracted, or
			// its channel died. Id-only, fire-and-forget; unknown ids (already
			// answered here) are no-ops. Run terminals stay the safety net (§8).
			if (this.#cards.delete(event.id)) this.#view.closeCard(event.id, undefined);
		},
		// --- compaction -------------------------------------------------------------
		// v15 envelope: begin → delta × N → (step × N → retried?)* → end/failed.
		compaction_begin: () => {
			this.#view.setStatus("compacting context…");
		},
		compaction_delta: () => {
			// The live summary block lands in M3; the end note carries the fact.
			log("compaction_delta received");
		},
		compaction_step: event => {
			// Summarization spend meters exactly like a completion_call's.
			this.#tree.addCompaction(event.id, event.usage.total_tokens);
			this.#meter(event.usage, event.cost);
		},
		compaction_retried: () => {
			// A discarded attempt: its deltas were never rendered, nothing drops.
		},
		compaction_end: event => {
			// tokens_after is the authoritative post-compaction context length.
			this.#contextUsed = event.tokens_after;
			this.#view.addNote("context compacted — history is now summary + retained tail", "info");
			this.#view.setStatus(this.#running ? "working — esc interrupts" : "idle");
			this.#emitFooter();
		},
		compaction_failed: event => {
			this.#view.addNote(`compaction failed: ${event.message}`, "error");
			this.#view.setStatus(this.#running ? "working — esc interrupts" : "idle");
		},
	};
}

/**
 * Lenient card parse for the two native templates (TOOLS.md):
 * `SelectOneCard`/`SelectAnyCard` → `{ title, body, options[{label}], free_text }`.
 * Anything else comes back undefined — the caller surfaces a cannot-answer
 * notice instead of guessing.
 */
function parseCard(id: string, uiType: string, payload: unknown): InteractionCard | undefined {
	if (uiType !== "native:select_one" && uiType !== "native:select_any") return undefined;
	if (typeof payload !== "object" || payload === null) return undefined;
	const p = payload as Record<string, unknown>;
	const options = Array.isArray(p.options)
		? p.options
				.filter((o): o is { label: string } => typeof o === "object" && o !== null && typeof (o as { label?: unknown }).label === "string")
				.map(o => o.label)
		: [];
	const freeText = p.free_text === true;
	// A card with neither options nor free text has no answer path.
	if (options.length === 0 && !freeText) return undefined;
	return {
		id,
		uiType,
		title: typeof p.title === "string" ? p.title : "(untitled ask)",
		body: typeof p.body === "string" ? p.body : "",
		options,
		freeText,
	};
}
