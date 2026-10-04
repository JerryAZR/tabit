/**
 * A protocol-faithful mock `tabit-core --json` for offline verification:
 * same report model (v19 — the backend speaks first, there is no
 * initialize handshake), same event vocabulary, same ordering rules (steer
 * boundaries, abort-before-terminal discards, interaction closing via
 * `interaction_settled` (v17) with run terminals as the net). Spoken as a child process (`node src/mock-backend.ts
 * --scenario X`) so the TUI-side tests exercise the real spawn/pipe/Ctrl+C
 * shape, not an in-process shortcut.
 *
 * Scenarios: basic | tools | subagent | steer | ask | replay | crash.
 * Every scenario also honors abort, interaction_response, and stdin-close.
 * Scope: the seven scenarios above are the contract — other commands
 * (`new_session`, `open_session`, `checkout`, `model`, `compact`,
 * `continue`) are total-semantics no-ops here, and the mock does not
 * model error kinds (unknown session, empty message) the real backend
 * produces; grow it per-scenario when a test needs them.
 */

import * as readline from "node:readline";

import { PROTOCOL_VERSION, type AvailableModel, type AvailableProvider, type MissingKeyProvider } from "./protocol.ts";

const scenario = process.argv.includes("--scenario") ? process.argv[process.argv.indexOf("--scenario") + 1] : "basic";

const BOOT = "0190MOCKBOOTSESSION0000000000000";
const CHILD = "0190MOOCKCHILDSESSION00000000000";

let out = "";
const emit = (frame: Record<string, unknown>) => {
	out += `${JSON.stringify(frame)}\n`;
};
const flush = () => {
	if (out) {
		process.stdout.write(out);
		out = "";
	}
};
const emitNow = (frame: Record<string, unknown>) => {
	emit(frame);
	flush();
};
const emitEvent = (stream: string | undefined, event: Record<string, unknown>) => {
	emitNow(stream ? { type: event.type, stream, ...event } : event);
};
const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

const usage = (i: number, o: number) => ({
	input_tokens: i,
	output_tokens: o,
	total_tokens: i + o,
	cached_input_tokens: 0,
	cache_creation_input_tokens: 0,
});

// v13: dollars stamped at commit from the rates in effect (the mock's
// rate card: $1/M in, $4/M out; cache legs are zero in the mock usage).
const cost = (i: number, o: number) => +((i * 1 + o * 4) / 1e6).toFixed(6);

const BASIC_MARKDOWN = [
	"# Mock run\n\n",
	"Here is a **streamed** answer with a code block:\n\n",
	"```rust\n",
	'fn main() {\n    println!("hello from the mock");\n}\n```\n\n',
	"And a list:\n\n",
	"- one\n- two\n- three\n\n",
	"The run will commit and finish. ",
];

let running = false;
let turnSeq = 0;
const nextId = (prefix: string) => `${prefix}${String(++turnSeq).padStart(4, "0")}`;

// v10 stamps: each scenario sets its run's start; terminals pair it with
// the terminal's own time. The child subagent run tracks its own start.
let runStartedAt = 0;
let childStartedAt = 0;
const ts = () => Date.now();

async function runTurn(stream: string, texts: string[], onMid?: () => Promise<void>): Promise<void> {
	const turn = nextId("turn-");
	emitEvent(stream, { type: "turn_started", id: turn, started_at_ms: ts() });
	for (const chunk of texts) {
		emitEvent(stream, { type: "text_delta", turn_id: turn, text: chunk });
		await sleep(20);
	}
	if (onMid) await onMid();
	emitEvent(stream, { type: "completion_call", turn_id: turn, usage: usage(421, 137), cost: cost(421, 137) });
	emitEvent(stream, { type: "turn_committed", id: turn, completed_at_ms: ts() });
}

async function basicRun(session: string): Promise<void> {
	runStartedAt = ts();
	emitEvent(session, {
		type: "user_message",
		text: "say the markdown thing",
		entry_id: nextId("entry-"),
	});
	await runTurn(session, BASIC_MARKDOWN);
	emitEvent(session, {
		type: "run_finished",
		output: "done",
		durable: true,
		started_at_ms: runStartedAt,
		completed_at_ms: ts(),
	});
	running = false;
}

const EDIT_DETAILS = {
	diff: {
		first_changed_line: 4,
		hunks: [
			{
				old_start: 3,
				old_lines: 3,
				new_start: 3,
				new_lines: 3,
				lines: [
					{ kind: "context", text: "fn main() {" },
					{ kind: "removed", text: '    println!("old");' },
					{ kind: "added", text: '    println!("new");' },
					{ kind: "context", text: "}" },
				],
			},
		],
	},
	outcomes: [{ index: 0, applied: true }],
};

async function toolsRun(session: string): Promise<void> {
	runStartedAt = ts();
	emitEvent(session, { type: "user_message", text: "edit then ask", entry_id: nextId("entry-") });
	const turn = nextId("turn-");
	emitEvent(session, { type: "turn_started", id: turn, started_at_ms: ts() });
	// Leading text and the tool call in the SAME synchronous emit block —
	// one pipe flush, the batching case the drain-before-boundary fix
	// exists for (deltas must land before the call re-shapes the model).
	emitEvent(session, { type: "text_delta", turn_id: turn, text: "I will edit then test. " });
	emitEvent(session, {
		type: "tool_call",
		turn_id: turn,
		name: "edit",
		call_id: "call-edit-1",
		internal_call_id: "int-edit-1",
		arguments: JSON.stringify({ path: "src/main.rs", edits: [{ old: "old", new: "new" }] }),
	});
	await sleep(60);
	emitEvent(session, {
		type: "tool_result",
		turn_id: turn,
		entry_id: nextId("entry-"),
		name: "edit",
		internal_call_id: "int-edit-1",
		content: "Edited src/main.rs (1 of 1 block applied; +1/-1 lines, first change at line 4)",
		status: { status: "success" },
		details: EDIT_DETAILS,
	});

	emitEvent(session, {
		type: "tool_call",
		turn_id: turn,
		name: "bash",
		call_id: "call-bash-1",
		internal_call_id: "int-bash-1",
		arguments: JSON.stringify({ command: "cargo test --workspace" }),
	});
	// The permission gate: a native:select_one card, open while the tool
	// waits. A run terminal would close it unanswered.
	const request = nextId("ask-");
	emitEvent(session, {
		type: "interaction_request",
		id: request,
		ui_type: "native:select_one",
		payload: {
			title: "Run command?",
			body: "cargo test --workspace",
			options: [{ label: "Allow" }, { label: "Always allow" }, { label: "Deny" }],
			free_text: true,
		},
	});
	await waitForResponse(session, request);
	emitEvent(session, {
		type: "tool_result",
		turn_id: turn,
		entry_id: nextId("entry-"),
		name: "bash",
		internal_call_id: "int-bash-1",
		content: "test result: ok. 42 passed; 0 failed",
		status: { status: "success" },
	});

	await runTurnTail(session, turn, "Edited and tested.");
	emitEvent(session, {
		type: "run_finished",
		output: "done",
		durable: true,
		started_at_ms: runStartedAt,
		completed_at_ms: ts(),
	});
	running = false;
}

async function runTurnTail(session: string, turn: string, text: string): Promise<void> {
	emitEvent(session, { type: "text_delta", turn_id: turn, text });
	emitEvent(session, { type: "completion_call", turn_id: turn, usage: usage(900, 210), cost: cost(900, 210) });
	emitEvent(session, { type: "turn_committed", id: turn, completed_at_ms: ts() });
}

async function subagentRun(session: string): Promise<void> {
	runStartedAt = ts();
	emitEvent(session, { type: "user_message", text: "delegate research", entry_id: nextId("entry-") });
	const turn = nextId("turn-");
	emitEvent(session, { type: "turn_started", id: turn, started_at_ms: ts() });
	emitEvent(session, {
		type: "tool_call",
		turn_id: turn,
		name: "subagent",
		call_id: "call-sub-1",
		internal_call_id: "int-sub-1",
		arguments: JSON.stringify({ task: "survey the diff landscape" }),
	});

	// The child: announced with `parent` + `parent_call`, its whole run on
	// its own stamp (v7's exact pairing).
	childStartedAt = ts();
	emitEvent(CHILD, {
		type: "session_opened",
		id: CHILD,
		path: "",
		cwd: process.cwd(),
		model: { provider: "mock", model: "child-model", thinking_level: null },
		resumed: false,
		parent: session,
		parent_call: "int-sub-1",
	});
	emitEvent(CHILD, { type: "user_message", text: "survey the diff landscape", entry_id: nextId("entry-") });
	await runTurn(CHILD, ["The child surveyed ", "three libraries ", "and preferred none.\n"]);
	emitEvent(CHILD, {
		type: "run_finished",
		output: "preferred none",
		durable: true,
		started_at_ms: childStartedAt,
		completed_at_ms: ts(),
	});

	emitEvent(session, {
		type: "tool_result",
		turn_id: turn,
		entry_id: nextId("entry-"),
		name: "subagent",
		internal_call_id: "int-sub-1",
		content: "The child surveyed three libraries and preferred none.",
		status: { status: "success" },
		// Pairing-only cargo (TOOLS.md): child_id + outcome; the child's
		// own stream carries the turns and usage.
		details: { child_id: CHILD, outcome: "completed" },
	});
	await runTurnTail(session, turn, "Delegated and summarized.");
	emitEvent(session, {
		type: "run_finished",
		output: "done",
		durable: true,
		started_at_ms: runStartedAt,
		completed_at_ms: ts(),
	});
	running = false;
}

// The free-text ask: a zero-option select_any. ask_user the tool is
// deleted (v10-era ruling); extension tools are the reference consumers
// now (TOOLS.md §templates) — this models one, `release-ask`, matching
// the extension announced at startup.
async function askRun(session: string): Promise<void> {
	runStartedAt = ts();
	emitEvent(session, { type: "user_message", text: "what next?", entry_id: nextId("entry-") });
	const turn = nextId("turn-");
	emitEvent(session, { type: "turn_started", id: turn, started_at_ms: ts() });
	emitEvent(session, {
		type: "tool_call",
		turn_id: turn,
		name: "release-ask",
		call_id: "call-ask-1",
		internal_call_id: "int-ask-1",
		arguments: JSON.stringify({ question: "Name the release" }),
	});
	const request = nextId("ask-");
	emitEvent(session, {
		type: "interaction_request",
		id: request,
		ui_type: "native:select_any",
		payload: { title: "Name the release", body: "zero options — type the answer", options: [], free_text: true },
	});
	const answer = (await waitForResponse(session, request)) as { selected?: string[]; text?: string } | undefined;
	emitEvent(session, {
		type: "tool_result",
		turn_id: turn,
		entry_id: nextId("entry-"),
		name: "release-ask",
		internal_call_id: "int-ask-1",
		content: `user answered: ${answer?.text ?? "(no text)"}`,
		status: { status: "success" },
	});
	await runTurnTail(session, turn, `The user picked ${answer?.text ?? "nothing"}. `);
	emitEvent(session, {
		type: "run_finished",
		output: "done",
		durable: true,
		started_at_ms: runStartedAt,
		completed_at_ms: ts(),
	});
	running = false;
}

const pendingResponses = new Map<string, { stream: string; resolve: (payload: unknown) => void }>();
function waitForResponse(stream: string, id: string): Promise<unknown> {
	return new Promise(resolve => {
		pendingResponses.set(id, { stream, resolve });
	});
}

// Steering: a long run whose boundary admits the queued message. The
// first turn streams slowly enough to leave a comfortable window for
// queueing a steer and aborting mid-run (tests drive by screen state).
async function steerRun(session: string): Promise<void> {
	runStartedAt = ts();
	emitEvent(session, { type: "user_message", text: "long task", entry_id: nextId("entry-") });
	const turn = nextId("turn-");
	emitEvent(session, { type: "turn_started", id: turn, started_at_ms: ts() });
	for (let i = 0; i < 40; i++) {
		emitEvent(session, { type: "text_delta", turn_id: turn, text: `tick ${i} ` });
		await sleep(60);
	}
	emitEvent(session, { type: "completion_call", turn_id: turn, usage: usage(421, 137), cost: cost(421, 137) });
	emitEvent(session, { type: "turn_committed", id: turn, completed_at_ms: ts() });
	const queuedMsg = takeQueued();
	if (queuedMsg) {
		emitEvent(session, { type: "user_message", text: queuedMsg.text, entry_id: queuedMsg.id });
	}
	await runTurn(session, ["Part two acknowledges the steer. "]);
	emitEvent(session, {
		type: "run_finished",
		output: "done",
		durable: true,
		started_at_ms: runStartedAt,
		completed_at_ms: ts(),
	});
	running = false;
}

const queued: Array<{ id: string; text: string }> = [];
function takeQueued(): { id: string; text: string } | undefined {
	return queued.shift();
}

// The mock's config world (v21): one usable provider, one configured
// but missing its key (the login widget's target). login/logout fold it.
const world: { usable: AvailableProvider[]; missing: MissingKeyProvider[] } = {
	usable: [
		{
			id: "mock",
			name: "Mock Provider",
			models: [
				{
					id: "mock-model",
					name: "Mock Model",
					context_window: 200000,
					max_tokens: 8192,
					cost: { input: 1, output: 4, cache_read: 0.1, cache_write: 0.4 },
					reasoning: true,
					input: ["text"],
					thinking_levels: ["low", "high"],
				},
			],
		},
	],
	missing: [{ id: "locked", name: "Locked Provider" }],
};
const LOCKED_MODELS: AvailableModel[] = [{ id: "locked-model", reasoning: false, input: ["text"], thinking_levels: [] }];

/** The catalog announcement — once at boot, then as the login/logout
 *  ack (last-wins re-announcement). */
const announceModels = () => {
	emitNow({
		type: "models_available",
		providers: world.usable.map(p => ({ ...p })),
		missing_keys: world.missing.map(({ id, name }) => ({ id, name })),
	});
};

// ---------------------------------------------------------------------------
// Boot (v19): the report is the first line, unprompted; the boot session's
// announcements follow — its stamped session_opened, its stamped skills
// catalog (v20, only-when-found), the backend-level catalogs, the resolved
// model record. Commands may flow from the frontend's first line on.
// ---------------------------------------------------------------------------

emitNow({ type: "report", protocol_version: PROTOCOL_VERSION });
emitEvent(BOOT, {
	type: "session_opened",
	id: BOOT,
	path: "",
	cwd: process.cwd(),
	model: { provider: "mock", model: "mock-model", thinking_level: null },
	resumed: false,
});
emitEvent(BOOT, {
	type: "skills_available",
	skills: [
		{ name: "commit", description: "draft a commit message", location: "~/.agents/skills", level: "user" },
		{ name: "review", description: "review a diff", location: ".agents/skills", level: "workspace" },
	],
});
emitNow({
	type: "sessions_available",
	sessions: [{ id: BOOT, created_at: new Date().toISOString(), entry_count: 0, path: "", cwd: process.cwd() }],
});
emitNow({
	type: "extensions_available",
	extensions: [
		{
			name: "release",
			version: "0.1.0",
			description: "release helpers",
			dir: "~/.tabit/extensions/release",
			status: "alive",
			tools: [
				{ name: "edit", description: "the extension's own edit" },
				{ name: "release-ask", description: "ask the user for the release name" },
			],
			hooks: [],
		},
		{
			name: "broken",
			version: "0.2.0",
			dir: "~/.tabit/extensions/broken",
			status: "dead",
			reason: "handshake refused: unknown hook point",
			tools: [],
			hooks: [],
		},
	],
	conflicts: [{ kind: "replaces_core", extension: "release", tool: "edit" }],
});
announceModels();
emitEvent(BOOT, {
	type: "model_changed",
	provider: "mock",
	model: "mock-model",
	thinking_level: null,
	// v11 facts: the resolved record (context meter denominator,
	// display name, per-million rates).
	context_window: 200000,
	name: "Mock Model",
	cost: { input: 1, output: 4, cache_read: 0.1, cache_write: 0.4 },
});

if (scenario === "replay") {
	// The resumed-boot replay pass (v19: default-on for a resumed boot —
	// no request flag exists anymore). A realistic pass (the shape
	// replay.rs emits): committed brackets with full-text deltas, tool
	// pairs — and NO run terminals, the detail the running-state gate
	// exists for.
	emitEvent(BOOT, { type: "replay_begin", total: 2 });
	emitEvent(BOOT, { type: "user_message", text: "resume me", entry_id: nextId("entry-") });
	const turn = nextId("turn-");
	emitEvent(BOOT, { type: "turn_started", id: turn, started_at_ms: ts() });
	emitEvent(BOOT, { type: "reasoning_delta", turn_id: turn, id: "r1", reasoning: "thinking it over " });
	emitEvent(BOOT, { type: "text_delta", turn_id: turn, text: "Resumed history, turn one. " });
	emitEvent(BOOT, {
		type: "tool_call",
		turn_id: turn,
		name: "read",
		call_id: "call-read-1",
		internal_call_id: "int-read-1",
		arguments: JSON.stringify({ path: "AGENTS.md" }),
	});
	emitEvent(BOOT, { type: "completion_call", turn_id: turn, usage: usage(100, 30), cost: cost(100, 30) });
	emitEvent(BOOT, { type: "turn_committed", id: turn, completed_at_ms: ts() });
	emitEvent(BOOT, {
		type: "tool_result",
		turn_id: turn,
		entry_id: nextId("entry-"),
		name: "read",
		internal_call_id: "int-read-1",
		content: "the file's contents",
		status: { status: "success" },
	});
	emitEvent(BOOT, { type: "replay_end" });
}

// ---------------------------------------------------------------------------
// Command intake
// ---------------------------------------------------------------------------

const crashArmed = scenario === "crash";
let crashed = false;

const stdin = readline.createInterface({ input: process.stdin });
stdin.on("line", line => {
	const trimmed = line.trim();
	if (trimmed === "") return;
	let frame: Record<string, unknown>;
	try {
		frame = JSON.parse(trimmed);
	} catch {
		emitNow({ type: "protocol_error", message: `unparseable line: ${trimmed.slice(0, 80)}` });
		return;
	}
	switch (frame.type) {
		case "message": {
			if (running) {
				const id = nextId("entry-");
				queued.push({ id, text: String(frame.text) });
				emitEvent(String(frame.session), { type: "message_queued", id, text: String(frame.text) });
			} else {
				running = true;
				if (crashArmed && !crashed) {
					// The crash path: internal error, exit 101, stderr report.
					crashed = true;
					process.stderr.write(
						"thread 'main' panicked at crates/tabit-session/src/outer_loop.rs:412:9:\n" +
							"internal invariant violated: mock crash on demand\n" +
							"note: run with `RUST_BACKTRACE=1` for a backtrace\n",
					);
					process.exit(101);
				}
				const starter =
					{
						basic: basicRun,
						tools: toolsRun,
						subagent: subagentRun,
						steer: steerRun,
						ask: askRun,
						crash: basicRun,
					}[scenario] ?? basicRun;
				starter(String(frame.session)).catch(err => {
					process.stderr.write(`mock scenario failed: ${err}\n`);
					process.exit(101);
				});
			}
			return;
		}
		case "abort": {
			const discarded = queued.splice(0);
			if (discarded.length > 0) {
				emitEvent(String(frame.session), {
					type: "messages_discarded",
					messages: discarded.map(m => ({ id: m.id, text: m.text })),
				});
			}
			if (running) {
				running = false;
				emitEvent(String(frame.session), {
					type: "run_aborted",
					output: "",
					started_at_ms: runStartedAt,
					completed_at_ms: ts(),
				});
			}
			return;
		}
		case "interaction_response": {
			const pending = pendingResponses.get(String(frame.id));
			if (pending) {
				pendingResponses.delete(String(frame.id));
				pending.resolve(frame.payload);
				// v17: the settle close follows the answer — fire-and-forget,
				// stamped like the request it closes.
				emitEvent(pending.stream, { type: "interaction_settled", id: String(frame.id) });
			}
			return;
		}
		case "login": {
			// v21: validate against config, then fold + re-announce (the ack).
			const provider = String(frame.provider);
			const key = String(frame.api_key ?? "");
			const known = world.usable.some(p => p.id === provider) || world.missing.some(p => p.id === provider);
			if (!known) {
				emitNow({ type: "error", kind: "auth", message: `unknown provider: ${provider}` });
				return;
			}
			if (key.trim() === "") {
				emitNow({ type: "error", kind: "auth", message: "empty key — nothing stored" });
				return;
			}
			const idx = world.missing.findIndex(p => p.id === provider);
			if (idx !== -1) {
				const [moved] = world.missing.splice(idx, 1);
				world.usable.push({ ...moved!, models: LOCKED_MODELS });
			}
			// Already-usable providers succeed unchanged (auth.toml wins over
			// the env case — the catalog stands); either way, re-announce.
			announceModels();
			return;
		}
		case "logout": {
			// v21: total and idempotent — unknown provider or absent key is a
			// no-op, still acked by the re-announced catalog.
			const provider = String(frame.provider);
			const idx = world.usable.findIndex(p => p.id === provider);
			if (idx !== -1) {
				const [moved] = world.usable.splice(idx, 1);
				world.missing.push({ id: moved!.id, name: moved!.name });
			}
			announceModels();
			return;
		}
		default:
			// Unknown commands are ignored by the mock (total semantics).
			return;
	}
});

stdin.on("close", () => {
	// The contractual death: stdin close kills the backend.
	process.exit(0);
});
