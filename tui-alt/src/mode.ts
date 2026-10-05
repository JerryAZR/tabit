/**
 * The interactive mode: a typed handler table over the backend's frames —
 * one handler per wire event, and the same table serves live traffic and
 * the replay pass (a pass arrives as the same vocabulary with live ids,
 * FRONTEND.md §7 — one transcript-rebuild path, no mode-side branching).
 *
 * M2: **one fold per stream**. Every session host (the root session and
 * each subagent child — children announce themselves with a
 * `session_opened` carrying `parent`/`parent_call`) gets a `StreamState`:
 * its transcript fold, its session facts, its run state, its pending
 * queue, its skills, its tree, its activity atom. Every stream folds
 * continuously, focus-independent (the backend runs every stream
 * independently, so the consumer must too). **Focus is a stream id** —
 * the view renders the focused stream's transcript; the editor's submit,
 * the session-scoped slash commands, and Esc's abort all route to it.
 *
 * Interaction cards are view-independent (owner ruling): the card slot
 * sits in the dock, not in any transcript; a card from any stream
 * surfaces immediately, labeled with its stream when non-root, and
 * answers route by the card's own stream.
 *
 * Transcript structure rule: **blocks are created on demand by their first
 * content** — `turn_started` allocates nothing, so a turn's text, thinking,
 * and tool rows stack in true wire-arrival order. Deltas coalesce into one
 * ordered buffer PER STREAM (text and reasoning interleaved as they
 * arrived), flushed every ~33 ms; within a flush batch the increments
 * apply in wire order.
 *
 * The report model (v19): the backend speaks first (`report`), there is no
 * handshake ack, and the routing key arrives ON the boot's stamped
 * `session_opened`. A frame naming an unannounced stream violates the
 * contract (a child's session_opened is always its first line) — logged
 * and dropped, never adopted.
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
	/** The focused stream's label (a child's task title) — undefined on
	 *  the root, where the footer shows no stream marker. */
	streamLabel?: string | undefined;
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
	/** The FOCUSED stream's run state (a child's run does not light the
	 *  status line while the root is focused — the list carries it). */
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

/** One row of the subagent list's projection — the widget's input
 *  (M2-DESIGN.md: the seam; the widget owns visibility policy). */
export interface SubagentEntry {
	stream: string;
	parent: string | undefined;
	/** The parent tool call's `task` argument (first line) — the user's
	 *  language; "subagent" until it resolves. */
	title: string;
	/** The activity atom: waiting > tool name / "N tools" > thinking >
	 *  running > idle > failed/aborted. */
	state: string;
	running: boolean;
	/** Wall-clock of the run terminal (undefined while a run is open or
	 *  none ever ran) — the widget's idle-hide clock. */
	idleSince: number | undefined;
}

/**
 * The rendering seam: implemented by the alt-screen root with live engine
 * components, and by tests as a recorder. Transcript-bound methods take
 * the stream they fold into (the view keeps one pane per stream, created
 * on demand); dock-level methods (status, footer, pending, skills,
 * cards, the subagent list) are stream-less — they always show the
 * FOCUSED stream's state, re-emitted on every focus change.
 */
export interface ModeView {
	/** Focus changed: render this stream's transcript (the view discards
	 *  the editor draft — there is no clear-all shortcut to save a
	 *  stranded one, M2-DESIGN.md). */
	showStream(stream: string): void;
	beginReplay(stream: string): void;
	endReplay(stream: string): void;
	/** The FOCUSED stream's skill catalog — the editor's `/` completion. */
	setSkills(skills: SkillInfo[]): void;
	addUser(stream: string, entryId: string, text: string): void;
	/** `stream` undefined = a backend-level note with no home stream —
	 *  lands in the root transcript. */
	addNote(stream: string | undefined, text: string, kind: "info" | "warn" | "error"): void;
	/** Lazy: creates the turn's assistant block on the first increment. */
	appendAssistantText(stream: string, turnId: string, text: string): void;
	/** Lazy: creates the thinking block on the first increment; the view
	 *  accumulates per `reasoningId` (same id = same block, FRONTEND.md §5). */
	appendReasoning(stream: string, turnId: string, reasoningId: string, text: string): void;
	addTool(stream: string, turnId: string, internalCallId: string, name: string, args: string | null): void;
	setToolResult(stream: string, internalCallId: string, content: string, ok: boolean, details?: unknown): void;
	/** Drop every provisional block the turn created (`turn_retried`). */
	removeTurn(stream: string, turnId: string): void;
	/** The FOCUSED stream's pending queue. */
	setPending(pending: PendingMessage[]): void;
	setStatus(text: string): void;
	setFooter(facts: FooterFacts): void;
	/** A card from any stream surfaces immediately (view-independent,
	 *  owner ruling); `streamLabel` names a non-root card's stream. */
	showCard(card: InteractionCard, streamLabel: string | undefined): void;
	closeCard(id: string, note: string | undefined): void;
	/** The subagent list's projection — re-emitted on any fold that could
	 *  change it (registration, run state, tools, cards). */
	setSubagents(entries: SubagentEntry[]): void;
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

/** Backend-level announcements arrive unstamped (they name no session) —
 *  the four events that fold globally instead of into a stream. */
const GLOBAL_TYPES: ReadonlySet<SessionEvent["type"]> = new Set([
	"sessions_available",
	"models_available",
	"providers_available",
	"extensions_available",
]);

type StampedEvent = Exclude<SessionEvent, { type: "sessions_available" | "models_available" | "providers_available" | "extensions_available" }>;
type StampedEventType = StampedEvent["type"];
type Handler<K extends StampedEventType> = (stream: StreamState, event: Extract<SessionEvent, { type: K }>) => void;

/** One coalesced delta, kept in arrival order so a flush replays the wire. */
type PendingDelta =
	| { kind: "text"; turnId: string; text: string }
	| { kind: "reasoning"; turnId: string; id: string; text: string };

/**
 * One stream's fold state — everything M1 held once, M2 holds per session
 * host. Created by the stream's `session_opened` (the announce is always
 * the first stamped line); lives until the process ends (the wire has no
 * reaped event — parked vs. swept is unobservable, M2-DESIGN.md's stated
 * limit).
 */
class StreamState {
	readonly id: string;
	readonly parent: string | undefined;
	readonly parentCall: string | undefined;
	/** The child's title — the parent tool call's `task` argument, first
	 *  line, resolved at announce time. Undefined on the root. */
	title: string | undefined;
	provider: string | undefined;
	model: string | undefined;
	modelName: string | undefined;
	contextWindow: number | undefined;
	path: string | undefined;
	cwd: string | undefined;
	resumed = false;
	rates: ModelCost | undefined;
	inputTokens = 0;
	outputTokens = 0;
	cachedInputTokens = 0;
	cacheCreationTokens = 0;
	cacheHitRate: number | undefined;
	cost: number | undefined;
	contextUsed: number | undefined;
	running = false;
	compacting = false;
	/** The last run terminal (undefined while a run is open or none ever
	 *  ran) + its wall-clock — the list's state word and idle-hide clock. */
	terminal: "completed" | "failed" | "aborted" | undefined;
	terminalAt = 0;
	/** Whether a replay pass is open on this stream (per-stream — a
	 *  replaying stream never suppresses another's live display). */
	replaying = false;
	pending: PendingMessage[] = [];
	skills: SkillInfo[] = [];
	readonly tree = new SessionTree();
	readonly deltas: PendingDelta[] = [];
	flushTimer: ReturnType<typeof setTimeout> | undefined;
	/** Tool calls awaiting their result (id → name, insertion order) —
	 *  the activity atom's tool leg. */
	readonly openCalls = new Map<string, string>();
	/** Every tool call's raw args by internal id — a child's title reads
	 *  its `parent_call`'s `task` from the parent's map. */
	readonly callArgs = new Map<string, string | null>();
	/** The open turn's latest delta kind — reasoning means "thinking". */
	lastDelta: "text" | "reasoning" | undefined;

	constructor(id: string, parent: string | undefined, parentCall: string | undefined) {
		this.id = id;
		this.parent = parent;
		this.parentCall = parentCall;
	}

	/** The activity atom (M2-DESIGN.md's priority: waiting > tools >
	 *  thinking > running > terminal/idle). */
	activity(waiting: boolean): string {
		if (waiting) return "waiting";
		if (this.openCalls.size > 0) {
			const names = [...this.openCalls.values()];
			return names.length === 1 ? names[0]! : `${names.length} tools`;
		}
		if (this.running) return this.lastDelta === "reasoning" ? "thinking" : "running";
		return this.terminal ?? "idle";
	}
}

export class InteractiveMode {
	readonly #backend: BackendLink;
	readonly #view: ModeView;
	/** The boot stream's id (a new_session replaces it — the old streams
	 *  stay in the map, kept per the memory ruling, and age out of the
	 *  list on their own). */
	#root: string | undefined;
	#focused: string | undefined;
	readonly #streams = new Map<string, StreamState>();
	/** The usable-model catalog (v21) — backend-level, folded last-wins
	 *  on every `models_available` (the boot announcement and each world
	 *  refresh). The picker's source when it lands. */
	#catalog: AvailableProvider[] = [];
	/** Every configured provider with its winning key source (v22) —
	 *  unconditional, folded last-wins with each world refresh. Login
	 *  targets re-derive from it (`auth: "none"`); `env` is display-only. */
	#providerStatuses: ProviderStatus[] = [];
	#keybindings: KeybindingFact[] = [];
	/** Open cards by id, each carrying the stream its answer routes to —
	 *  cards are view-independent (owner ruling), never per-transcript. */
	readonly #cards = new Map<string, { card: InteractionCard; stream: string }>();
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

	/** The boot stream (the session the frontend was opened on). */
	get activeSession(): string | undefined {
		return this.#root;
	}

	/** The stream the editor, the session commands, and Esc route to. */
	get focusedStream(): string | undefined {
		return this.#focused;
	}

	#focusedState(): StreamState | undefined {
		return this.#focused === undefined ? undefined : this.#streams.get(this.#focused);
	}

	/** Resolved app keybindings, for `/help`. Called by the entry after
	 *  the registry is installed. */
	setKeybindings(facts: KeybindingFact[]): void {
		this.#keybindings = facts;
	}

	/** Whether the FOCUSED stream has a run in flight (Esc's abort leg). */
	get running(): boolean {
		return this.#focusedState()?.running ?? false;
	}

	/** The usable-model catalog, latest announcement (v21). */
	get modelsCatalog(): readonly AvailableProvider[] {
		return this.#catalog;
	}

	/** Every configured provider with its winning key source (v22). */
	get providerStatuses(): readonly ProviderStatus[] {
		return this.#providerStatuses;
	}

	/** The FOCUSED stream's register (provider + model ids), for the
	 *  picker's current marker. Undefined when it has no selection (v21's
	 *  zero-config boot). */
	get currentSelection(): { provider: string; model: string } | undefined {
		const s = this.#focusedState();
		if (s?.provider === undefined || s.model === undefined) return undefined;
		return { provider: s.provider, model: s.model };
	}

	/** The FOCUSED stream's session tree — read-only for the view. */
	get tree(): SessionTree {
		return this.#focusedState()?.tree ?? new SessionTree();
	}

	/** Rewind the FOCUSED stream to an entry (the tree card's enter).
	 *  Fire-and-forget like every command; `checked_out` + the replay pass
	 *  are the outcome. */
	checkout(entryId: string): void {
		const s = this.#focusedState();
		if (s !== undefined) this.#backend.checkout(s.id, entryId);
	}

	get replaying(): boolean {
		return this.#focusedState()?.replaying ?? false;
	}

	/**
	 * Switch focus to a stream (the list's Enter, Esc's parent walk). The
	 * view swaps the transcript pane and discards the editor draft; the
	 * mode re-emits every dock-level fact from the newly focused stream.
	 */
	focusStream(stream: string): void {
		if (stream === this.#focused || !this.#streams.has(stream)) return;
		this.#focused = stream;
		this.#view.showStream(stream);
		this.#emitFocusedState();
	}

	/** Esc's law (M2-DESIGN.md): abort the focused stream's run; when
	 *  idle and not root, walk to the parent (generic — never hardcoded
	 *  to root). Returns whether it acted — root-idle Esc falls through
	 *  to the editor (its autocomplete dismiss). */
	escape(): boolean {
		const s = this.#focusedState();
		if (s === undefined) return false;
		if (s.running) {
			this.#backend.abort(s.id);
			return true;
		}
		if (s.id !== this.#root) {
			this.focusStream(s.parent ?? this.#root ?? s.id);
			return true;
		}
		return false;
	}

	/** Editor submit: slash space first (`/compact [guidance]` is a wire
	 *  command — the guidance rides the frame as this invocation's
	 *  directives; `/help` lists keys and commands; `exit`/`quit` end the
	 *  TUI; a skill name formats the wire's invocation tag — the
	 *  `<skill name="…"/>` marker, expanded by the backend at the message
	 *  door, the interim UX until skill chips land), else a plain message
	 *  to the FOCUSED stream. */
	submit(text: string): void {
		const s = this.#focusedState();
		if (s === undefined || text === "") return;
		if (text.startsWith("/")) {
			const body = text.slice(1).trim();
			const space = body.indexOf(" ");
			const name = space === -1 ? body : body.slice(0, space);
			const args = space === -1 ? "" : body.slice(space + 1).trim();
			const entry = this.#slashEntries().find(candidate => candidate.name === name);
			if (entry !== undefined) entry.run(args);
			else this.#view.addNote(undefined, `/${name} is not a command — /help lists them`, "warn");
			return;
		}
		this.#backend.message(s.id, text);
	}

	/**
	 * The slash command set — the one home. The dropdown reads it and the
	 * interpreter runs it, so a command cannot exist in one and not the
	 * other (the /help-shipped-but-unlisted miss was exactly that split).
	 * Every entry carries its behavior; `kind` marks the skill entries for
	 * the dropdown's type column. Session-scoped commands target the
	 * focused stream; backend-level ones (login/logout) name no session.
	 */
	slashCommands(): Array<{ name: string; description: string; kind: "command" | "skill" }> {
		return this.#slashEntries().map(({ name, description, kind }) => ({ name, description, kind }));
	}

	#slashEntries(): Array<{ name: string; description: string; kind: "command" | "skill"; run: (args: string) => void }> {
		const focused = () => this.#focusedState();
		return [
			{
				name: "compact",
				description: "summarize the context now",
				kind: "command",
				run: args => {
					const s = focused();
					if (s === undefined) return;
					const directives = args === "" ? undefined : args;
					this.#backend.compact(s.id, directives);
				},
			},
			{ name: "help", description: "list keys and commands", kind: "command", run: () => this.#showHelp() },
			{ name: "login", description: "store a provider API key", kind: "command", run: () => this.onLogin?.() },
			{ name: "logout", description: "remove a stored provider key", kind: "command", run: () => this.onLogout?.() },
			{ name: "model", description: "switch the model", kind: "command", run: () => this.onModel?.() },
			{ name: "tree", description: "browse the session tree, rewind to an entry", kind: "command", run: () => this.onTree?.() },
			{ name: "exit", description: "quit the TUI (shuts the backend down)", kind: "command", run: () => this.onQuit?.() },
			{ name: "quit", description: "quit the TUI (shuts the backend down)", kind: "command", run: () => this.onQuit?.() },
			// A skill invocation formats the wire's tag (FRONTEND.md §5): the
			// backend expands resolvable tags at the message door; the rest of
			// the text rides along as the message. No arguments on the tag
			// itself. Skills are per-session (v20) — the focused stream's.
			...(focused()?.skills ?? []).map(skill => ({
				name: skill.name,
				description: skill.description,
				kind: "skill" as const,
				run: (args: string) => {
					const s = focused();
					if (s === undefined) return;
					const tag = `<skill name="${skill.name}"/>`;
					this.#backend.message(s.id, args === "" ? tag : `${tag} ${args}`);
				},
			})),
		];
	}

	#showHelp(): void {
		const names = this.#slashEntries().map(entry => `/${entry.name}`).join(" ·  ");
		this.#view.addNote(undefined, `commands ·  ${names} — @ paths complete files`, "info");
		for (const fact of this.#keybindings) {
			this.#view.addNote(undefined, `keys ·  ${fact.description}: ${fact.keys.join(" / ")}`, "info");
		}
	}

	/** The picker's select: fire the `model` command at the FOCUSED stream
	 *  (a state write at receive; `model_changed` answers). */
	switchModel(provider: string, model: string): void {
		const s = this.#focusedState();
		if (s !== undefined) this.#backend.setModel(s.id, provider, model);
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

	/** Ctrl+C's leg: abort the FOCUSED stream's run (Esc's law lives in
	 *  `escape()` — abort-or-parent-walk). */
	interrupt(): void {
		const s = this.#focusedState();
		if (s?.running === true) this.#backend.abort(s.id);
	}

	/** Answer a card: `selected` echoes the chosen labels (exactly one
	 *  for select_one, 0..n for select_any), `text` carries the note when
	 *  the card invited one. The answer routes to the card's own stream —
	 *  focus is irrelevant to it. */
	answerCard(id: string, selected: string[], text: string | null): void {
		const entry = this.#cards.get(id);
		if (entry === undefined) return;
		this.#cards.delete(id);
		this.#backend.interactionResponse(entry.stream, id, { selected, text });
		this.#view.closeCard(id, undefined);
		this.#emitSubagents();
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
			this.#view.addNote(undefined, `unknown frame${parsed.type ? ` (${parsed.type})` : ""} — logged, connection kept`, "warn");
			return;
		}
		// A backend-level error before the boot session opened is the
		// startup-failure shape (v19): the report, this one unstamped error
		// carrying the reason (config problems carry the setup guide), then
		// a nonzero exit. Display the reason and die — respawn is the fix.
		if (parsed.stream === undefined && parsed.event.type === "error" && this.#root === undefined) {
			this.onFatal?.(parsed.event.message);
			return;
		}
		const event = parsed.event;
		// The stream's own announce is the registration act — always the
		// stream's first stamped line, for the root and children alike.
		if (event.type === "session_opened") {
			const state = this.#registerStream(event);
			this.#handlers.session_opened(state, event);
			this.#emitSubagents();
			return;
		}
		// Backend-level announcements name no stream — they fold globally.
		// An unstamped arrival of any OTHER type is the wire being lenient
		// (a post-boot backend-level error is the live case): fold it into
		// the root stream, never drop it silently.
		if (parsed.stream === undefined || GLOBAL_TYPES.has(event.type)) {
			if (GLOBAL_TYPES.has(event.type)) {
				this.#handleGlobal(event);
				return;
			}
			const root = this.#root === undefined ? undefined : this.#streams.get(this.#root);
			if (root === undefined) {
				log(`unstamped ${event.type} before the boot's announce — dropped`);
				return;
			}
			const rootHandler = this.#handlers[event.type as StampedEventType] as ((stream: StreamState, event: SessionEvent) => void) | undefined;
			rootHandler?.(root, event);
			this.#emitSubagents();
			return;
		}
		const stream = this.#streams.get(parsed.stream);
		if (stream === undefined) {
			// A frame for an unannounced stream violates the contract (the
			// announce is always first) — log and drop, never adopt.
			log(`frame for unannounced stream ${parsed.stream}: ${event.type} — dropped`);
			return;
		}
		const handler = this.#handlers[event.type as StampedEventType] as ((stream: StreamState, event: SessionEvent) => void) | undefined;
		handler?.(stream, event);
		// The list re-projects on any stamped fold — cheap (streams are few)
		// and never stale (run state, tools, and cards all feed the atom).
		this.#emitSubagents();
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
		this.#view.addNote(undefined, `protocol error: ${frame.message}`, "error");
	}

	/** The registration act. A rootless announce is the boot session — or
	 *  a new_session's replacement root; either way it takes the focus. A
	 *  child's announce resolves its title from the parent's `parent_call`
	 *  tool call (the `task` argument's first line). */
	#registerStream(event: Extract<SessionEvent, { type: "session_opened" }>): StreamState {
		const state = new StreamState(event.id, event.parent, event.parent_call);
		if (event.parent !== undefined && event.parent_call !== undefined) {
			const parentArgs = this.#streams.get(event.parent)?.callArgs.get(event.parent_call);
			state.title = taskTitle(parentArgs);
		}
		this.#streams.set(event.id, state);
		if (event.parent === undefined) {
			this.#root = event.id;
			this.#focused = event.id;
			this.#view.showStream(event.id);
		}
		return state;
	}

	/** Re-emit every dock-level fact from the focused stream — the footer,
	 *  the status line, the pending queue, the skill catalog. Called on
	 *  focus change and by the folds that mutate them. */
	#emitFocusedState(): void {
		const s = this.#focusedState();
		if (s === undefined) return;
		this.#view.setFooter({
			streamLabel: s.id === this.#root ? undefined : (s.title ?? s.id),
			session: s.id,
			path: s.path,
			cwd: s.cwd,
			model: s.model,
			modelName: s.modelName,
			contextWindow: s.contextWindow,
			resumed: s.resumed,
			inputTokens: s.inputTokens,
			outputTokens: s.outputTokens,
			cachedInputTokens: s.cachedInputTokens,
			cacheCreationTokens: s.cacheCreationTokens,
			cacheHitRate: s.cacheHitRate,
			cost: s.cost,
			contextUsed: s.contextUsed,
			rates: s.rates,
			running: s.running,
		});
		this.#view.setStatus(this.#statusText(s));
		this.#view.setPending(s.pending);
		this.#view.setSkills(s.skills);
	}

	#statusText(s: StreamState): string {
		if (s.compacting) return "compacting context…";
		if (s.running) return "working — esc interrupts";
		return "idle";
	}

	/** Emit the status line when the FOCUSED stream's status changed (a
	 *  background child's run state never lights the status line — the
	 *  list carries it). */
	#emitStatus(s: StreamState): void {
		if (s.id === this.#focused) this.#view.setStatus(this.#statusText(s));
	}

	/** Emit the footer when the FOCUSED stream's facts changed. */
	#emitFooter(s: StreamState): void {
		if (s.id === this.#focused) this.#emitFocusedState();
	}

	/** The one usage fold: a completion_call and a compaction_step meter
	 *  identically (v15 — compaction spend is spend), so totals are one
	 *  fold over both event kinds. Also tracks the freshest context
	 *  length (`compaction_end` overwrites with its authoritative
	 *  `tokens_after`) and the latest request's cache hit rate. */
	#meter(s: StreamState, usage: Usage, cost?: number): void {
		s.inputTokens += usage.input_tokens;
		s.outputTokens += usage.output_tokens;
		s.cachedInputTokens += usage.cached_input_tokens;
		s.cacheCreationTokens += usage.cache_creation_input_tokens;
		const promptLegs = usage.input_tokens + usage.cached_input_tokens + usage.cache_creation_input_tokens;
		s.cacheHitRate = promptLegs > 0 ? (usage.cached_input_tokens / promptLegs) * 100 : undefined;
		if (cost !== undefined) s.cost = (s.cost ?? 0) + cost;
		s.contextUsed = usage.total_tokens;
		this.#emitFooter(s);
	}

	/** The subagent list's projection (M2-DESIGN.md): every non-root
	 *  stream, its title, its activity atom. The widget owns visibility
	 *  policy (the 60s idle-hide); the projection states facts. */
	#emitSubagents(): void {
		const entries: SubagentEntry[] = [];
		for (const s of this.#streams.values()) {
			if (s.parent === undefined) continue;
			const waiting = [...this.#cards.values()].some(card => card.stream === s.id);
			entries.push({
				stream: s.id,
				parent: s.parent,
				title: s.title ?? "subagent",
				state: s.activity(waiting),
				running: s.running,
				idleSince: s.running || s.terminal === undefined ? undefined : s.terminalAt,
			});
		}
		this.#view.setSubagents(entries);
	}

	#scheduleFlush(s: StreamState): void {
		s.flushTimer ??= setTimeout(() => this.#flush(s), FLUSH_MS);
	}

	#flush(s: StreamState): void {
		if (s.flushTimer !== undefined) {
			clearTimeout(s.flushTimer);
			s.flushTimer = undefined;
		}
		for (const delta of s.deltas) {
			if (delta.kind === "text") this.#view.appendAssistantText(s.id, delta.turnId, delta.text);
			else this.#view.appendReasoning(s.id, delta.turnId, delta.id, delta.text);
		}
		s.deltas.length = 0;
	}

	#dropTurnDeltas(s: StreamState, turnId: string): void {
		// A retried turn's still-buffered deltas must never paint: drop them
		// before anything flushes. Other turns' deltas stay buffered.
		for (let index = s.deltas.length - 1; index >= 0; index--) {
			if (s.deltas[index]!.turnId === turnId) s.deltas.splice(index, 1);
		}
	}

	/** Run terminals close the cards OF THAT STREAM (a card blocks its own
	 *  run; other streams' cards are untouched). */
	#closeStreamCards(s: StreamState, reason: string): void {
		for (const [id, entry] of this.#cards) {
			if (entry.stream === s.id) {
				this.#cards.delete(id);
				this.#view.closeCard(id, reason);
			}
		}
	}

	/** The four backend-level announcements — unstamped, session-less. */
	#handleGlobal(event: SessionEvent): void {
		switch (event.type) {
			case "sessions_available":
				this.#view.addNote(undefined, `${event.sessions.length} session(s) on disk`, "info");
				return;
			case "extensions_available":
				for (const ext of event.extensions) {
					if (ext.status === "dead") this.#view.addNote(undefined, `extension ${ext.name} is dead: ${ext.reason ?? "unknown reason"}`, "error");
				}
				for (const conflict of event.conflicts) {
					this.#view.addNote(
						undefined,
						`extension conflict (${conflict.kind}): ${conflict.extension}'s tool "${conflict.tool}"${conflict.incumbent ? ` displaces ${conflict.incumbent}` : ""}`,
						"warn",
					);
				}
				this.#view.addNote(undefined, `${event.extensions.length} extension(s) loaded`, "info");
				return;
			case "models_available":
				// v21: the usable-model picker catalog — backend-level, always
				// emitted (even empty), re-announced on world change, last-wins.
				// The setup-state warning rides providers_available (v22), which
				// follows in the same act with the full key-source picture.
				this.#catalog = event.providers;
				if (event.providers.length === 0) return;
				{
					const models = event.providers.reduce((n, p) => n + p.models.length, 0);
					this.#view.addNote(undefined, `${models} model(s) across ${event.providers.length} provider(s)`, "info");
				}
				return;
			case "providers_available":
				// v22: unconditional, one act with models_available. The setup
				// predicate (§3.2) branches the first-run teaching here.
				this.#providerStatuses = event.providers;
				if (event.providers.length === 0) {
					this.#view.addNote(undefined, "no providers configured at this backend — write ~/.tabit/providers.toml and restart", "warn");
					return;
				}
				{
					const missing = event.providers.filter(p => p.auth === "none");
					if (this.#catalog.length === 0) {
						this.#view.addNote(undefined, `no usable models — missing keys for: ${missing.map(p => p.id).join(", ")}`, "warn");
						return;
					}
					if (missing.length > 0) {
						this.#view.addNote(undefined, `no key for: ${missing.map(p => p.id).join(", ")} — login fixes in-app`, "info");
					}
				}
				return;
			default:
				// An unstamped arrival of a stamped-vocabulary event: the wire
				// is confused, but lenient parse holds — log, never crash.
				log(`unstamped arrival of ${event.type} — dropped`);
		}
	}

	readonly #handlers: { [K in StampedEventType]: Handler<K> } = {
		// --- run lifecycle ---------------------------------------------------
		user_message: (s, event) => {
			if (!s.replaying) {
				s.running = true;
				s.terminal = undefined;
				this.#emitStatus(s);
				this.#emitFooter(s);
			}
			s.tree.addUser(event.entry_id, event.text);
			this.#view.addUser(s.id, event.entry_id, event.text);
			const next = s.pending.filter(p => p.id !== event.entry_id);
			if (next.length !== s.pending.length) {
				s.pending = next;
				if (s.id === this.#focused) this.#view.setPending(s.pending);
			}
		},
		run_finished: (s, event) => {
			this.#flush(s);
			s.running = false;
			s.terminal = "completed";
			s.terminalAt = Date.now();
			this.#emitStatus(s);
			this.#emitFooter(s);
			if (!event.durable) {
				this.#view.addNote(s.id, "log writes are degraded — this run may not survive a crash", "warn");
			}
			this.#closeStreamCards(s, "run finished");
		},
		run_aborted: (s, event) => {
			this.#flush(s);
			s.running = false;
			s.terminal = "aborted";
			s.terminalAt = Date.now();
			this.#emitStatus(s);
			this.#emitFooter(s);
			this.#closeStreamCards(s, "run aborted");
			this.#view.addNote(s.id, "run aborted", "warn");
			if (event.output !== "") log(`aborted with partial output (${event.output.length} chars)`);
		},
		run_failed: (s, event) => {
			this.#flush(s);
			s.running = false;
			s.terminal = "failed";
			s.terminalAt = Date.now();
			this.#emitStatus(s);
			this.#emitFooter(s);
			this.#view.addNote(s.id, `run failed (${event.kind}): ${event.message}`, "error");
			// Pending messages survive a failure — they drain into the next run.
		},
		// --- queueing ------------------------------------------------------------
		message_queued: (s, event) => {
			s.pending = [...s.pending, { id: event.id, text: event.text }];
			if (s.id === this.#focused) this.#view.setPending(s.pending);
		},
		messages_discarded: (s, event) => {
			s.pending = [];
			if (s.id === this.#focused) this.#view.setPending(s.pending);
			this.#view.addNote(s.id, `${event.messages.length} queued message(s) discarded — the backend kept no copy`, "warn");
		},
		// --- transcript ----------------------------------------------------------
		// turn_started allocates nothing: blocks are created on demand by
		// their first content, so document order == wire arrival order.
		turn_started: (s, event) => {
			s.tree.openTurn(event.id);
			s.lastDelta = undefined;
		},
		text_delta: (s, event) => {
			s.tree.appendTurnText(event.turn_id, event.text);
			s.lastDelta = "text";
			s.deltas.push({ kind: "text", turnId: event.turn_id, text: event.text });
			this.#scheduleFlush(s);
		},
		reasoning_delta: (s, event) => {
			s.lastDelta = "reasoning";
			s.deltas.push({ kind: "reasoning", turnId: event.turn_id, id: event.id, text: event.reasoning });
			this.#scheduleFlush(s);
		},
		turn_committed: () => {},
		turn_retried: (s, event) => {
			s.tree.retryTurn(event.turn_id);
			this.#dropTurnDeltas(s, event.turn_id);
			this.#view.removeTurn(s.id, event.turn_id);
			this.#view.addNote(s.id, "turn discarded before commit — a retry follows", "info");
		},
		turn_truncated: (s) => {
			this.#view.addNote(s.id, "turn hit the provider output limit — the run continues", "warn");
		},
		tool_call: (s, event) => {
			s.tree.noteToolCall(event.internal_call_id, event.name, event.arguments);
			s.openCalls.set(event.internal_call_id, event.name);
			s.callArgs.set(event.internal_call_id, event.arguments);
			this.#view.addTool(s.id, event.turn_id, event.internal_call_id, event.name, event.arguments);
		},
		tool_result: (s, event) => {
			s.tree.addTool(event.entry_id, event.internal_call_id, event.name, event.content);
			s.openCalls.delete(event.internal_call_id);
			this.#view.setToolResult(s.id, event.internal_call_id, event.content, event.status.status === "success", event.details);
		},
		completion_call: (s, event) => {
			// v12: the per-turn report is the only home — every request's
			// usage counts, aborted and failed runs included, and replay
			// passes re-deliver it so sums survive resume.
			this.#meter(s, event.usage, event.cost);
		},
		native_item: () => {
			// Provider-native, live-only, never replayed — nothing to render yet.
			log("native_item received");
		},
		// --- errors / durability ---------------------------------------------------
		error: (s, event) => {
			if (event.kind === "persist_degraded") {
				this.#view.addNote(s.id, `log writes degraded — ${event.pending ?? "?"} record(s) pending flush`, "warn");
				return;
			}
			if (event.kind === "persist_recovered") {
				this.#view.addNote(s.id, "log writes recovered", "info");
				return;
			}
			// kind "model" is a degradation, not a death (§6): a fallback
			// named, or the zero-config teaching note — the session runs on.
			// kind "auth" (v21) is a failed login/logout — an error note.
			this.#view.addNote(s.id, `error (${event.kind}): ${event.message}`, event.kind === "model" ? "warn" : "error");
		},
		// --- replay ------------------------------------------------------------------
		replay_begin: (s) => {
			this.#flush(s);
			s.replaying = true;
			s.deltas.length = 0;
			s.tree.closeTurn();
			this.#view.beginReplay(s.id);
		},
		replay_end: (s) => {
			s.replaying = false;
			this.#view.endReplay(s.id);
		},
		checked_out: (s, event) => {
			// The head moves now; the full re-render arrives as the following
			// replay brackets (which also re-walk the tree's shared prefix).
			s.tree.checkout(event.entry_id);
			this.#view.addNote(s.id, `rewound to ${event.entry_id}`, "info");
		},
		// --- session-scoped announcements -----------------------------------------
		skills_available: (s, event) => {
			// v20: stamped with the session's stream, announced as each session
			// becomes visible — each stream keeps its own catalog; the editor's
			// completion shows the FOCUSED stream's.
			s.skills = event.skills.map(skill => ({ name: skill.name, description: skill.description ?? "" }));
			if (s.id === this.#focused) this.#view.setSkills(s.skills);
			this.#view.addNote(s.id, `${event.skills.length} skill(s) loaded: ${event.skills.map(skill => skill.name).join(", ")}`, "info");
		},
		session_opened: (s, event) => {
			// Registration already happened (#registerStream); here the facts
			// fold. v21: null = no selection (the zero-config boot) — footer
			// facts stay undefined until the first model command lands one.
			s.provider = event.model?.provider;
			s.model = event.model?.model;
			// Empty path = ephemeral session (nothing on disk to open).
			s.path = event.path === "" ? undefined : event.path;
			s.cwd = event.cwd === "" ? undefined : event.cwd;
			s.resumed = event.resumed;
			// Per-session facts reset here: the model_changed ahead of the
			// following replay restates the resolved record, and the pass
			// re-delivers completion_calls, so sums rebuild from history.
			s.modelName = undefined;
			s.contextWindow = undefined;
			s.rates = undefined;
			s.inputTokens = 0;
			s.outputTokens = 0;
			s.cachedInputTokens = 0;
			s.cacheCreationTokens = 0;
			s.cacheHitRate = undefined;
			s.cost = undefined;
			s.contextUsed = undefined;
			// v20: skills are per-session — announced only when discovery found
			// at least one, so absence is unambiguous and the old session's list
			// must not survive into this one.
			s.skills = [];
			if (s.id === this.#focused) this.#view.setSkills(s.skills);
			if (event.parent === undefined) {
				// A new root (boot or new_session) replaced the session: the
				// tree card's fold starts over. Children stay in the map (the
				// memory ruling) and age out of the list on their own.
				s.tree.reset();
				// Boot facts have landed — the connection is no longer
				// "connecting".
				this.#emitFocusedState();
			} else {
				this.#view.addNote(this.#root, `subagent started: ${s.title ?? s.id}`, "info");
			}
		},
		model_changed: (s, event) => {
			s.provider = event.provider;
			s.model = event.model;
			s.modelName = event.name;
			s.contextWindow = event.context_window;
			s.rates = event.cost;
			this.#emitFooter(s);
		},
		// --- interactions ------------------------------------------------------------------
		interaction_request: (s, event) => {
			const card = parseCard(event.id, event.ui_type, event.payload);
			if (card === undefined) {
				// Unknown ui_type or malformed payload: surface it, never
				// fabricate an answer (FRONTEND.md §8). The run stays blocked
				// until its terminal closes the card.
				this.#view.addNote(s.id, `cannot answer card (${event.ui_type}) — unsupported shape`, "warn");
				return;
			}
			this.#cards.set(card.id, { card, stream: s.id });
			// Cards are view-independent: any stream's card surfaces now,
			// labeled with its stream when it isn't the root's.
			this.#view.showCard(card, s.id === this.#root ? undefined : (s.title ?? s.id));
		},
		interaction_settled: (s, event) => {
			// v17: the settle close — the request was answered, retracted, or
			// its channel died. Id-only, fire-and-forget; unknown ids (already
			// answered here) are no-ops. Run terminals stay the safety net (§8).
			if (this.#cards.delete(event.id)) this.#view.closeCard(event.id, undefined);
		},
		// --- compaction -------------------------------------------------------------
		// v15 envelope: begin → delta × N → (step × N → retried?)* → end/failed.
		compaction_begin: (s) => {
			s.compacting = true;
			this.#emitStatus(s);
		},
		compaction_delta: () => {
			// The live summary block lands in M3; the end note carries the fact.
			log("compaction_delta received");
		},
		compaction_step: (s, event) => {
			// Summarization spend meters exactly like a completion_call's.
			s.tree.addCompaction(event.id, event.usage.total_tokens);
			this.#meter(s, event.usage, event.cost);
		},
		compaction_retried: () => {
			// A discarded attempt: its deltas were never rendered, nothing drops.
		},
		compaction_end: (s, event) => {
			s.compacting = false;
			// tokens_after is the authoritative post-compaction context length.
			s.contextUsed = event.tokens_after;
			this.#view.addNote(s.id, "context compacted — history is now summary + retained tail", "info");
			this.#emitStatus(s);
			this.#emitFooter(s);
		},
		compaction_failed: (s, event) => {
			s.compacting = false;
			this.#view.addNote(s.id, `compaction failed: ${event.message}`, "error");
			this.#emitStatus(s);
		},
	};
}

/** A child stream's title from its parent tool call's arguments: the
 *  `task` field's first line, truncated. Unparseable → undefined (the
 *  projection falls back to "subagent"). */
function taskTitle(args: string | null | undefined): string | undefined {
	if (args == null) return undefined;
	try {
		const parsed: unknown = JSON.parse(args);
		if (typeof parsed !== "object" || parsed === null) return undefined;
		const task = (parsed as { task?: unknown }).task;
		if (typeof task !== "string" || task.trim() === "") return undefined;
		const firstLine = task.trim().split("\n")[0]!;
		return firstLine.length > 60 ? `${firstLine.slice(0, 59)}…` : firstLine;
	} catch {
		return undefined;
	}
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
