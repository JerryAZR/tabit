/**
 * The tabit frontend wire vocabulary (FRONTEND.md, protocol v20), mirrored
 * from `crates/tabit-protocol` as TypeScript types, plus the lenient parse
 * a hand-rolled client owes the contract: JSON.parse first, switch on
 * known `type`s to a typed union, and hand everything unrecognized back as
 * an `unknown` frame — logged by the caller, never swallowed, never fatal
 * (FRONTEND.md §2: skipping unknown types is the forward-compat path).
 *
 * v8: `skills_available` startup announcement. v9: `extensions_available` —
 * the extension catalog with provenance and load-time conflict reports.
 * v10: `session_created` deleted (the `new_session` outcome is a stamped
 * `session_opened`), `run_failed` typed with `kind`, Unix-ms timestamps on
 * the turn brackets and run terminals, and `session_opened.parent_call`
 * pairs a child with its spawning tool call. v11: `model_changed` carries
 * the resolved model facts (`context_window`, `name`, `cost` — optional;
 * absent means the config does not state one). v12: per-turn usage is
 * complete (`completion_call` carries the full five-field `Usage`) and
 * `run_finished`'s aggregate is deleted — per-turn is the home, sums are
 * the frontend's; v13 put the recorded dollars on `completion_call`
 * (`cost` — stamped at commit from the rates in effect, so history
 * survives rate changes; never re-derived client-side). v14 put the
 * compaction pass's facts on the finish event; v15 reshaped the family
 * into the invocation envelope — `compaction_begin` → `compaction_step` × N
 * → `compaction_end`/`compaction_failed`, with `compaction_retried` for
 * discarded attempts; a step's `usage`/`cost` meter exactly like a
 * `completion_call`'s. v16: `session_opened.cwd`, `sessions_available`
 * rows' `path`/`cwd`, real `compact` directives. v17: `interaction_settled`
 * — the settle close for interaction cards. v18: event frames may carry
 * `origin` (extension attribution) and `ttl` (the node net's hop budget —
 * consumers ignore it). v19: the report model — the backend speaks first
 * (`report { protocol_version }`); `initialize`/`initialize_ack`/
 * `initialize_rejected` are deleted (the frontend is the version check and
 * owns the kill; startup failures are the report, one unstamped `error`,
 * and a nonzero exit); replay brackets renamed `replay_begin { total }` /
 * `replay_end`, default-on for resumed boots. v20: `skills_available` is
 * session-level — stamped with the session's stream, announced per
 * session; fold per stream. v21: `models_available` — the backend-level
 * usable-model catalog (providers with per-model facts, plus
 * `missing_keys` — the login widget's targets), always emitted even
 * empty, re-announced when the world changes (last-wins); the
 * backend-level `login`/`logout` commands (the re-announced catalog is
 * the ack, `error { kind: "auth" }` the failure); zero config boots —
 * `session_opened.model` is nullable (null = no selection, serialized
 * present).
 */

export const PROTOCOL_VERSION = 21;

// ---------------------------------------------------------------------------
// Commands (frontend → backend). Fire-and-forget; outcomes arrive as
// events. Session-scoped commands always name their session.
// ---------------------------------------------------------------------------

export type SessionCommand =
	| { type: "message"; session: string; text: string }
	| { type: "abort"; session: string }
	| { type: "continue"; session: string }
	| { type: "new_session" }
	| { type: "open_session"; id: string }
	| { type: "checkout"; session: string; entry_id: string }
	| { type: "compact"; session: string; directives?: string }
	| {
			type: "model";
			session: string;
			provider: string;
			model: string;
			thinking_level?: string | null;
	  }
	| { type: "interaction_response"; session: string; id: string; payload: unknown }
	| { type: "login"; provider: string; api_key: string }
	| { type: "logout"; provider: string };

/** v19: a client line IS a command — the initialize handshake is deleted
 *  and commands may flow from the frontend's first line. */
export type ClientFrame = SessionCommand;

// ---------------------------------------------------------------------------
// Server frames (backend → frontend)
// ---------------------------------------------------------------------------

export type ServerControlFrame =
	| { type: "report"; protocol_version: number }
	| { type: "protocol_error"; message: string };

export interface ModelSelection {
	provider: string;
	model: string;
	thinking_level?: string | null;
}

/** One model in a `models_available` provider row (v21): the id a `model`
 *  command addresses plus the facts config states. Every optional field
 *  follows the v11 rule — absent means the config does not state it,
 *  never zero. `thinking_levels` carries the dial's ordered NAMES; empty
 *  when the model has no dial, and `null` is always a legal selection on
 *  top (the provider/model default). */
export interface AvailableModel {
	id: string;
	name?: string;
	context_window?: number;
	max_tokens?: number;
	cost?: ModelCost;
	/** Whether the model produces reasoning output. */
	reasoning: boolean;
	/** The accepted input modalities, lowercase ("text", "image"). */
	input: string[];
	thinking_levels: string[];
}

/** One usable provider in the `models_available` catalog (v21) — usable
 *  means a resolvable key or `keyless = true`; unusable providers cross
 *  only as `missing_keys` identities. Alphabetical id order; display
 *  sorting is the frontend's. */
export interface AvailableProvider {
	id: string;
	name?: string;
	models: AvailableModel[];
}

/** A configured provider failing the usable predicate (v21) — identity
 *  only; the login widget's targets. */
export interface MissingKeyProvider {
	id: string;
	name?: string;
}

/** Per-million-token pricing, USD — the mirror of tabit-config's cost record. */
export interface ModelCost {
	input: number;
	output: number;
	cache_read: number;
	cache_write: number;
}

/** The resolved model-record facts a `model_changed` carries (v11): all
 *  optional — absent means the config does not state one, never zero. */
export interface ModelFacts {
	context_window?: number;
	name?: string;
	cost?: ModelCost;
}

export interface Usage {
	input_tokens: number;
	output_tokens: number;
	total_tokens: number;
	cached_input_tokens: number;
	cache_creation_input_tokens: number;
}

export interface AvailableSession {
	id: string;
	created_at: string;
	entry_count: number;
	/** The session log file's path and working directory (v16). */
	path: string;
	cwd: string;
}

/** One discovered skill in the `skills_available` announcement (v8). */
export interface AvailableSkill {
	name: string;
	description: string;
	location: string;
	/** `user` (home) or `workspace` (cwd) — which discovery source won. */
	level: string;
}

/** One declared tool in `AvailableExtension`. */
export interface AvailableExtensionTool {
	name: string;
	description: string;
}

/** One discovered extension in the `extensions_available` announcement (v9). */
export interface AvailableExtension {
	name: string;
	version: string;
	description?: string;
	/** The package's directory on the host — the provenance. */
	dir: string;
	/** `alive` or `dead` (refused handshake, failed scan, or death since). */
	status: string;
	/** Why a dead extension is dead. */
	reason?: string;
	tools: AvailableExtensionTool[];
	hooks: string[];
}

/** One load-time name-assembly report riding `extensions_available`. */
export interface ExtensionConflict {
	kind: "replaces_core" | "refused_peer" | string;
	extension: string;
	tool: string;
	incumbent?: string;
}

export interface DiscardedMessage {
	id: string;
	text: string;
}

export type ToolResultStatus = { status: "success" } | { status: "failed"; exit_code?: number };

/**
 * The edit tool's presentation cargo (crates/tabit-tools/src/diff.rs):
 * `{ diff: { first_changed_line, hunks: [...] }, outcomes: [...] }` with
 * hunk lines tagged context/removed/added. Dispatch on the tool name;
 * degrade to `content` when absent or unrecognized.
 */
export interface EditDetails {
	diff?: {
		first_changed_line?: number | null;
		hunks?: Array<{
			old_start: number;
			old_lines: number;
			new_start: number;
			new_lines: number;
			lines: Array<{ kind: "context" | "removed" | "added"; text: string }>;
		}>;
	};
	outcomes?: Array<{ index: number; applied: boolean; reason?: string }>;
}

/**
 * The subagent tool result's pairing-only cargo (TOOLS.md): `child_id`
 * matches the child's `session_opened` id (and its `parent_call` link);
 * `turns`/`usage` were deleted — the child's own stream carries them.
 */
export interface SubagentDetails {
	child_id: string;
	outcome: string;
}

/** One session event, tag-flattened exactly as it rides the wire. */
export type SessionEvent =
	| { type: "run_aborted"; output: string; started_at_ms: number; completed_at_ms: number }
	| { type: "user_message"; text: string; entry_id: string }
	| { type: "message_queued"; id: string; text: string }
	| { type: "messages_discarded"; messages: DiscardedMessage[] }
	| { type: "turn_started"; id: string; started_at_ms: number }
	| { type: "turn_committed"; id: string; completed_at_ms: number }
	| { type: "turn_retried"; turn_id: string }
	| { type: "turn_truncated"; turn_id: string }
	| { type: "text_delta"; turn_id: string; text: string }
	| { type: "reasoning_delta"; turn_id: string; id: string; reasoning: string }
	| {
			type: "tool_call";
			turn_id: string;
			name: string;
			call_id: string;
			internal_call_id: string;
			arguments: string | null;
	  }
	| {
			type: "tool_result";
			turn_id: string;
			entry_id: string;
			name: string;
			internal_call_id: string;
			content: string;
			status: ToolResultStatus;
			details?: unknown;
	  }
	| { type: "completion_call"; turn_id: string; usage: Usage; cost?: number }
	| {
			type: "run_finished";
			output: string;
			durable: boolean;
			started_at_ms: number;
			completed_at_ms: number;
	  }
	| { type: "run_failed"; message: string; kind: string; started_at_ms: number; completed_at_ms: number }
	| { type: "error"; kind: string; message: string; pending?: number }
	| { type: "replay_begin"; total: number }
	| { type: "replay_end" }
	| { type: "checked_out"; entry_id: string; base_id: string | null }
	| { type: "sessions_available"; sessions: AvailableSession[] }
	| { type: "skills_available"; skills: AvailableSkill[] }
	| {
			type: "extensions_available";
			extensions: AvailableExtension[];
			conflicts: ExtensionConflict[];
	  }
	| {
			type: "session_opened";
			id: string;
			path: string;
			/** The session's working directory (v16): the boot's is the
			 *  backend's cwd, a child's is its spawn cwd. */
			cwd: string;
			/** Nullable (v21): null = no selection (nothing usable at this
			 *  backend — the zero-config boot); serialized present, never
			 *  skipped. */
			model: ModelSelection | null;
			resumed: boolean;
			parent?: string;
			/** The spawning tool call's internal id — pairs the child with the exact open tool_call. */
			parent_call?: string;
	  }
	| { type: "models_available"; providers: AvailableProvider[]; missing_keys: MissingKeyProvider[] }
	| {
			type: "model_changed";
			provider: string;
			model: string;
			thinking_level?: string | null;
			context_window?: number;
			name?: string;
			cost?: ModelCost;
	  }
	| { type: "native_item"; turn_id: string; item: unknown }
	| { type: "interaction_request"; id: string; ui_type: string; payload: unknown }
	| { type: "interaction_settled"; id: string }
	| { type: "compaction_begin" }
	| { type: "compaction_delta"; text: string }
	| { type: "compaction_step"; id: string; usage: Usage; cost?: number }
	| { type: "compaction_retried" }
	| { type: "compaction_end"; tokens_after: number }
	| { type: "compaction_failed"; message: string };

/** A stamped event line; `stream` (the session id) is absent for backend-level
 *  frames. `origin` (v18) attributes an extension's emission; `ttl` (v18) is
 *  the node net's hop budget — consumers ignore it. */
export interface EventFrame {
	stream?: string;
	origin?: string;
	ttl?: number;
	event: SessionEvent;
}

export type ParsedServerFrame =
	| { kind: "control"; frame: ServerControlFrame }
	| { kind: "event"; stream?: string; origin?: string; event: SessionEvent }
	| { kind: "unknown"; raw: string; type?: string };

const CONTROL_TYPES = new Set(["report", "protocol_error"]);

const EVENT_TYPES = new Set([
	"run_aborted",
	"user_message",
	"message_queued",
	"messages_discarded",
	"turn_started",
	"turn_committed",
	"turn_retried",
	"turn_truncated",
	"text_delta",
	"reasoning_delta",
	"tool_call",
	"tool_result",
	"completion_call",
	"run_finished",
	"run_failed",
	"error",
	"replay_begin",
	"replay_end",
	"checked_out",
	"sessions_available",
	"skills_available",
	"extensions_available",
	"session_opened",
	"model_changed",
	"models_available",
	"native_item",
	"interaction_request",
	"interaction_settled",
	"compaction_begin",
	"compaction_delta",
	"compaction_step",
	"compaction_retried",
	"compaction_end",
	"compaction_failed",
]);

/**
 * Parse one backend line. Lenient by doctrine: anything that fails to
 * parse, or carries an unrecognized `type`, comes back as `unknown` —
 * the caller reports it and moves on; the connection never breaks on
 * account of a frame from a newer backend.
 */
export function parseServerFrame(line: string): ParsedServerFrame | null {
	const trimmed = line.trim();
	if (trimmed === "") return null;
	let value: unknown;
	try {
		value = JSON.parse(trimmed);
	} catch {
		return { kind: "unknown", raw: trimmed };
	}
	if (typeof value !== "object" || value === null) return { kind: "unknown", raw: trimmed };
	const obj = value as Record<string, unknown>;
	const type = typeof obj.type === "string" ? obj.type : undefined;
	if (type === undefined) return { kind: "unknown", raw: trimmed };
	if (CONTROL_TYPES.has(type)) return { kind: "control", frame: obj as unknown as ServerControlFrame };
	if (EVENT_TYPES.has(type)) {
		// `origin` (v18) is attribution worth keeping; `ttl` (v18) is the
		// node net's routing tripwire — consumers ignore it by ruling, so
		// it is stripped here rather than leaked into the event.
		const { stream, origin, ttl: _ttl, ...event } = obj;
		return {
			kind: "event",
			stream: typeof stream === "string" ? stream : undefined,
			origin: typeof origin === "string" ? origin : undefined,
			event: event as SessionEvent,
		};
	}
	return { kind: "unknown", raw: trimmed, type };
}

/** Serialize a client frame to its wire line (LF-terminated by the writer). */
export function toWireLine(frame: ClientFrame): string {
	return JSON.stringify(frame);
}
