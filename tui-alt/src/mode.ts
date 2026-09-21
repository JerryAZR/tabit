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
 */

import { log } from "./log";
import { PROTOCOL_VERSION } from "./protocol";
import type { ModelCost, ParsedServerFrame, ServerControlFrame, SessionEvent, Usage } from "./protocol";

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

/**
 * The rendering seam: implemented by the alt-screen root with live engine
 * components, and by tests as a recorder. The mode never holds rendered
 * text beyond what the ordered delta buffer carries.
 */
export interface ModeView {
	beginReplay(): void;
	endReplay(): void;
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
	interactionResponse(session: string, id: string, payload: unknown): void;
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
	#model: string | undefined;
	#modelName: string | undefined;
	#contextWindow: number | undefined;
	#resumed = false;
	#inputTokens = 0;
	#outputTokens = 0;
	#cachedInputTokens = 0;
	#cacheCreationTokens = 0;
	#cacheHitRate: number | undefined;
	#cost: number | undefined;
	#contextUsed: number | undefined;
	#rates: ModelCost | undefined;
	#pending: PendingMessage[] = [];
	readonly #cards = new Map<string, InteractionCard>();
	readonly #deltas: PendingDelta[] = [];
	#flushTimer: ReturnType<typeof setTimeout> | undefined;
	/** Set on `initialize_rejected`; the entry turns it into the exit path. */
	onFatal: ((reason: string) => void) | undefined;

	constructor(backend: BackendLink, view: ModeView) {
		this.#backend = backend;
		this.#view = view;
		this.#view.setStatus("connecting…");
	}

	get activeSession(): string | undefined {
		return this.#session;
	}

	get running(): boolean {
		return this.#running;
	}

	get replaying(): boolean {
		return this.#replaying;
	}

	/** Editor submit: steers when running, starts a run when idle. */
	submit(text: string): void {
		if (!this.#session || text === "") return;
		this.#backend.message(this.#session, text);
	}

	interrupt(): void {
		if (this.#session && this.#running) this.#backend.abort(this.#session);
	}

	answerCard(id: string, optionIndex: number): void {
		const card = this.#cards.get(id);
		if (!card) return;
		const label = card.options[optionIndex];
		// Out-of-range or option-less cards get no fabricated answer — the
		// render side surfaces them as cannot-answer notices (FRONTEND.md §8).
		if (label === undefined) return;
		this.#cards.delete(id);
		this.#backend.interactionResponse(this.#session!, id, { selected: [label], text: null });
		this.#view.closeCard(id, undefined);
	}

	handleFrame(parsed: ParsedServerFrame): void {
		if (parsed.kind === "control") return this.#handleControl(parsed.frame);
		if (parsed.kind === "unknown") {
			log(`unknown frame${parsed.type ? ` type=${parsed.type}` : " (unparseable)"}: ${parsed.raw}`);
			this.#view.addNote(`unknown frame${parsed.type ? ` (${parsed.type})` : ""} — logged, connection kept`, "warn");
			return;
		}
		// Child traffic: the announcement (session_opened with a parent)
		// renders a note; everything else on a foreign stamp logs only.
		if (parsed.stream !== undefined && parsed.stream !== this.#session) {
			log(`child stream ${parsed.stream}: ${parsed.event.type}`);
			return;
		}
		const event = parsed.event;
		const handler = this.#handlers[event.type] as ((event: SessionEvent) => void) | undefined;
		handler?.(event);
	}

	#handleControl(frame: ServerControlFrame): void {
		if (frame.type === "initialize_ack") {
			// The contract's exact-match rule, frontend side: an ack from a
			// backend speaking any other version means our event vocabulary
			// is wrong for this pipe — fail loud, never limp on unknown frames.
			if (frame.protocol_version !== PROTOCOL_VERSION) {
				this.onFatal?.(
					`protocol version mismatch: backend speaks v${frame.protocol_version}, this frontend speaks v${PROTOCOL_VERSION}`,
				);
				return;
			}
			// One ack per connection; the boot session's facts arrive on its
			// own session_opened — this only gives us the routing key.
			this.#session ??= frame.session_id;
			return;
		}
		if (frame.type === "initialize_rejected") {
			this.onFatal?.(frame.reason);
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
		turn_started: () => {},
		text_delta: event => {
			this.#deltas.push({ kind: "text", turnId: event.turn_id, text: event.text });
			this.#scheduleFlush();
		},
		reasoning_delta: event => {
			this.#deltas.push({ kind: "reasoning", turnId: event.turn_id, id: event.id, text: event.reasoning });
			this.#scheduleFlush();
		},
		turn_committed: () => {},
		turn_retried: event => {
			this.#dropTurnDeltas(event.turn_id);
			this.#view.removeTurn(event.turn_id);
			this.#view.addNote("turn discarded before commit — a retry follows", "info");
		},
		turn_truncated: () => {
			this.#view.addNote("turn hit the provider output limit — the run continues", "warn");
		},
		tool_call: event => {
			this.#view.addTool(event.turn_id, event.internal_call_id, event.name, event.arguments);
		},
		tool_result: event => {
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
			this.#view.addNote(`error (${event.kind}): ${event.message}`, "error");
		},
		// --- replay ------------------------------------------------------------------
		replay_started: () => {
			this.#flush();
			this.#replaying = true;
			this.#deltas.length = 0;
			this.#view.beginReplay();
		},
		replay_done: () => {
			this.#replaying = false;
			this.#view.endReplay();
		},
		checked_out: event => {
			// The full re-render arrives as the following replay brackets.
			this.#view.addNote(`rewound to ${event.entry_id}`, "info");
		},
		// --- announcements / config ----------------------------------------------------
		sessions_available: event => {
			this.#view.addNote(`${event.sessions.length} session(s) on disk`, "info");
		},
		skills_available: event => {
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
			this.#model = event.model.model;
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
			this.#emitFooter();
			// Boot facts have landed — the connection is no longer "connecting".
			if (!this.#running) this.#view.setStatus("idle");
		},
		model_changed: event => {
			this.#model = event.model;
			this.#modelName = event.name;
			this.#contextWindow = event.context_window;
			this.#rates = event.cost;
			this.#emitFooter();
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
