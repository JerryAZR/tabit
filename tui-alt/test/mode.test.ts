/**
 * The mode against a scripted frame feed — the spike's stub-context
 * pattern: hand-emit typed events, assert on the view recorder. No TUI
 * boots here; rendering is the engine's job, routing/state is the mode's.
 * The recorder logs every block mutation in order so the coalescer's
 * wire-order guarantee is assertable.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { setTimeout as sleep } from "node:timers/promises";

import { InteractiveMode, type BackendLink, type FooterFacts, type InteractionCard, type ModeView, type PendingMessage, type SkillInfo, type SubagentEntry } from "../src/mode.ts";
import { parseServerFrame, PROTOCOL_VERSION, type ParsedServerFrame } from "../src/protocol.ts";

const SESSION = "0199aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CHILD = "0199bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

class FakeBackend implements BackendLink {
	readonly sent: Array<{
		kind: string;
		session?: string;
		id?: string;
		payload?: unknown;
		text?: string;
		directives?: string;
		entryId?: string;
		provider?: string;
		model?: string;
		apiKey?: string;
	}> = [];
	message(session: string, text: string): void {
		this.sent.push({ kind: "message", session, text });
	}
	abort(session: string): void {
		this.sent.push({ kind: "abort", session });
	}
	compact(session: string, directives?: string): void {
		// The real Backend omits the field when no directives are given
		// (`...(directives !== undefined ? { directives } : {})`) — the
		// double mirrors the wire, so deep equality stays honest.
		this.sent.push({ kind: "compact", session, ...(directives !== undefined ? { directives } : {}) });
	}
	checkout(session: string, entryId: string): void {
		this.sent.push({ kind: "checkout", session, entryId });
	}
	setModel(session: string, provider: string, model: string): void {
		this.sent.push({ kind: "model", session, provider, model });
	}
	interactionResponse(session: string, id: string, payload: unknown): void {
		this.sent.push({ kind: "interaction_response", session, id, payload });
	}
	login(provider: string, apiKey: string): void {
		this.sent.push({ kind: "login", provider, apiKey });
	}
	logout(provider: string): void {
		this.sent.push({ kind: "logout", provider });
	}
}

class RecordingView implements ModeView {
	replayBegun = 0;
	replayEnded = 0;
	users: Array<{ entryId: string; text: string }> = [];
	/** Parallel streams for multi-stream assertions (M2) — userStreams[i]
	 *  is the stream users[i] folded into. */
	userStreams: Array<string | undefined> = [];
	notes: Array<{ text: string; kind: string }> = [];
	noteStreams: Array<string | undefined> = [];
	assistantText = new Map<string, string>();
	assistantStreams = new Map<string, string>();
	reasoningText = new Map<string, string>();
	removedTurns: string[] = [];
	tools = new Map<string, { stream: string; turnId: string; name: string; args: string | null; content?: string; ok?: boolean; details?: unknown }>();
	pending: PendingMessage[] = [];
	status = "";
	footer: FooterFacts | undefined;
	cards: InteractionCard[] = [];
	cardLabels: Array<string | undefined> = [];
	closed: Array<{ id: string; note: string | undefined }> = [];
	shownStreams: string[] = [];
	subagents: SubagentEntry[] = [];
	/** Every block mutation in apply order: `text:t1`, `reasoning:t1:r1`, `tool:i1`, `user:e1`. */
	order: string[] = [];

	showStream(stream: string): void {
		this.shownStreams.push(stream);
	}
	beginReplay(_stream: string): void {
		this.replayBegun++;
		this.assistantText.clear();
		this.reasoningText.clear();
		this.tools.clear();
	}
	endReplay(_stream: string): void {
		this.replayEnded++;
	}
	addUser(stream: string, entryId: string, text: string): void {
		this.users.push({ entryId, text });
		this.userStreams.push(stream);
		this.order.push(`user:${entryId}`);
	}
	addNote(stream: string | undefined, text: string, kind: "info" | "warn" | "error"): void {
		this.notes.push({ text, kind });
		this.noteStreams.push(stream);
	}
	appendAssistantText(stream: string, turnId: string, text: string): void {
		this.assistantText.set(turnId, (this.assistantText.get(turnId) ?? "") + text);
		this.assistantStreams.set(turnId, stream);
		this.order.push(`text:${turnId}`);
	}
	appendReasoning(_stream: string, turnId: string, reasoningId: string, text: string): void {
		const key = `${turnId}:${reasoningId}`;
		this.reasoningText.set(key, (this.reasoningText.get(key) ?? "") + text);
		this.order.push(`reasoning:${reasoningId}`);
	}
	addTool(stream: string, turnId: string, internalCallId: string, name: string, args: string | null): void {
		this.tools.set(internalCallId, { stream, turnId, name, args });
		this.order.push(`tool:${internalCallId}`);
	}
	setToolResult(_stream: string, internalCallId: string, content: string, ok: boolean, details?: unknown): void {
		const tool = this.tools.get(internalCallId);
		if (tool !== undefined) {
			tool.content = content;
			tool.ok = ok;
			tool.details = details;
		}
	}
	removeTurn(_stream: string, turnId: string): void {
		this.removedTurns.push(turnId);
		this.assistantText.delete(turnId);
		for (const key of [...this.reasoningText.keys()]) {
			if (key.startsWith(`${turnId}:`)) this.reasoningText.delete(key);
		}
		for (const [id, tool] of [...this.tools]) {
			if (tool.turnId === turnId) this.tools.delete(id);
		}
	}
	setPending(pending: PendingMessage[]): void {
		this.pending = pending;
	}
	setStatus(text: string): void {
		this.status = text;
	}
	setFooter(facts: FooterFacts): void {
		this.footer = facts;
	}
	skills: SkillInfo[] = [];
	setSkills(skills: SkillInfo[]): void {
		this.skills = skills;
	}
	showCard(card: InteractionCard, streamLabel: string | undefined): void {
		this.cards.push(card);
		this.cardLabels.push(streamLabel);
	}
	closeCard(id: string, note: string | undefined): void {
		this.closed.push({ id, note });
	}
	setSubagents(entries: SubagentEntry[]): void {
		this.subagents = entries;
	}
}

function harness() {
	const backend = new FakeBackend();
	const view = new RecordingView();
	const mode = new InteractiveMode(backend, view);
	const feed = (event: Record<string, unknown>, stream: string | undefined = SESSION): void => {
		mode.handleFrame({ kind: "event", stream, event } as ParsedServerFrame);
	};
	const control = (frame: Extract<ParsedServerFrame, { kind: "control" }>["frame"]): void => {
		mode.handleFrame({ kind: "control", frame } as ParsedServerFrame);
	};
	return { backend, view, mode, feed, control };
}

function boot(
	control: (frame: Extract<ParsedServerFrame, { kind: "control" }>["frame"]) => void,
	feed: (event: Record<string, unknown>, stream?: string | undefined) => void,
	session = SESSION,
): void {
	// The report model (v19): the backend speaks first, and the routing key
	// arrives ON the boot's stamped session_opened — no handshake ack exists.
	control({ type: "report", protocol_version: PROTOCOL_VERSION });
	feed({ type: "session_opened", id: session, path: "", cwd: "", model: { provider: "p", model: "m1" }, resumed: false }, session);
}

/** The recorded-dollars assertions: absent stays absent, present is close. */
function costCloseTo(actual: number | undefined, expected: number): void {
	assert.ok(actual !== undefined, "expected a recorded cost");
	assert.ok(Math.abs(actual - expected) < 1e-9, `${actual} not close to ${expected}`);
}

describe("InteractiveMode", () => {
	test("the boot's session_opened mints the routing key; submits address the active session", () => {
		const { backend, view, mode, feed, control } = harness();
		assert.strictEqual(view.status, "connecting…");
		boot(control, feed);
		mode.submit("hello");
		assert.deepStrictEqual(backend.sent, [{ kind: "message", session: SESSION, text: "hello" }]);
		// Pre-boot submits have no session to address — dropped, not sent.
		const fresh = harness();
		fresh.mode.submit("lost");
		assert.deepStrictEqual(fresh.backend.sent, []);
	});

	test("a live run: liveness on user_message, coalesced deltas, tool lifecycle, per-turn usage", async () => {
		const { view, mode, feed, control } = harness();
		boot(control, feed);
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		feed({ type: "model_changed", provider: "p", model: "m1", thinking_level: null, context_window: 200000, name: "Model One", cost: { input: 1, output: 4, cache_read: 0.1, cache_write: 0.4 } });
		assert.strictEqual(view.footer?.path, "/w"); // the editor's completion root
		feed({ type: "user_message", entry_id: "e1", text: "hi" });
		assert.strictEqual(view.footer?.running, true);
		assert.strictEqual(view.status, "working — esc interrupts");
		assert.deepStrictEqual(view.users, [{ entryId: "e1", text: "hi" }]);
		assert.strictEqual(view.footer?.modelName, "Model One");
		assert.strictEqual(view.footer?.contextWindow, 200000);
		assert.deepStrictEqual(view.footer?.rates, { input: 1, output: 4, cache_read: 0.1, cache_write: 0.4 });

		feed({ type: "turn_started", id: "t1", started_at_ms: 1 });
		feed({ type: "reasoning_delta", turn_id: "t1", id: "r1", reasoning: "think" });
		feed({ type: "text_delta", turn_id: "t1", text: "Hel" });
		feed({ type: "text_delta", turn_id: "t1", text: "lo" });
		await sleep(45); // flush timer (33 ms)
		assert.strictEqual(view.assistantText.get("t1"), "Hello");
		assert.strictEqual(view.reasoningText.get("t1:r1"), "think");

		feed({ type: "tool_call", turn_id: "t1", name: "bash", call_id: "c1", internal_call_id: "i1", arguments: "{\"cmd\":\"ls\"}" });
		assert.partialDeepStrictEqual(view.tools.get("i1"), { name: "bash", args: "{\"cmd\":\"ls\"}" });
		feed({ type: "tool_result", turn_id: "t1", entry_id: "e2", name: "bash", internal_call_id: "i1", content: "ok", status: { status: "success" }, details: { trivial: true } });
		assert.partialDeepStrictEqual(view.tools.get("i1"), { content: "ok", ok: true, details: { trivial: true } });

		// v12/v13: the per-turn report is the summing home — the run
		// terminal carries no usage at all.
		feed({ type: "completion_call", turn_id: "t1", usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15, cached_input_tokens: 3, cache_creation_input_tokens: 2 }, cost: 0.00003 });
		assert.strictEqual(view.footer?.inputTokens, 10);
		assert.strictEqual(view.footer?.outputTokens, 5);
		assert.strictEqual(view.footer?.cachedInputTokens, 3);
		assert.strictEqual(view.footer?.cacheCreationTokens, 2);
		costCloseTo(view.footer?.cost, 0.00003);

		feed({ type: "run_finished", output: "lo", durable: true, started_at_ms: 1, completed_at_ms: 2 });
		assert.strictEqual(view.footer?.running, false);
		assert.strictEqual(view.footer?.inputTokens, 10);
		assert.strictEqual(mode.running, false);
	});

	test("the session log path fact: real path flows, ephemeral stays undefined", () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		feed({ type: "session_opened", id: SESSION, path: "C:\\proj\\.tabit\\s\\a.jsonl", model: { provider: "p", model: "m1" }, resumed: false });
		assert.strictEqual(view.footer?.path, "C:\\proj\\.tabit\\s\\a.jsonl"); // the log file — session UI data, not a cwd

		const ephemeral = harness();
		boot(ephemeral.control, ephemeral.feed);
		ephemeral.feed({ type: "session_opened", id: SESSION, path: "", model: { provider: "p", model: "m1" }, resumed: false });
		assert.strictEqual(ephemeral.view.footer?.path, undefined);
	});

	test("usage accounting: sums across turns and terminals, absent costs stay absent, replay re-sums after reset", async () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		const usage = (i: number, o: number, c = 0, w = 0) => ({
			input_tokens: i,
			output_tokens: o,
			total_tokens: i + o,
			cached_input_tokens: c,
			cache_creation_input_tokens: w,
		});
		// Two requests in one turn, then an abort mid-second-turn: every
		// request counts (v12 — aborted and failed runs included).
		feed({ type: "completion_call", turn_id: "t1", usage: usage(100, 20, 50, 10), cost: 0.0002 });
		feed({ type: "completion_call", turn_id: "t1", usage: usage(30, 5) }); // no cost reported
		feed({ type: "user_message", entry_id: "e1", text: "again" });
		feed({ type: "completion_call", turn_id: "t2", usage: usage(7, 2), cost: 0.0001 });
		feed({ type: "run_aborted", output: "", started_at_ms: 1, completed_at_ms: 2 });
		assert.strictEqual(view.footer?.inputTokens, 137);
		assert.strictEqual(view.footer?.outputTokens, 27);
		assert.strictEqual(view.footer?.cachedInputTokens, 50);
		assert.strictEqual(view.footer?.cacheCreationTokens, 10);
		costCloseTo(view.footer?.cost, 0.0003); // absent cost skipped, not zeroed

		// A resume-style boot: facts reset, then the replay pass re-delivers
		// the history's completion_calls through the same handler.
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: true });
		assert.strictEqual(view.footer?.inputTokens, 0);
		assert.strictEqual(view.footer?.cost, undefined);
		assert.strictEqual(view.footer?.resumed, true);
		feed({ type: "replay_begin", total: 1 });
		feed({ type: "completion_call", turn_id: "t1", usage: usage(100, 20), cost: 0.0002 });
		feed({ type: "replay_end" });
		await sleep(45);
		assert.strictEqual(view.footer?.inputTokens, 100);
		assert.strictEqual(view.footer?.outputTokens, 20);
		costCloseTo(view.footer?.cost, 0.0002);
	});

	test("blocks appear in wire order: lazy creation, no pre-allocation at turn_started", async () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		feed({ type: "turn_started", id: "t9", started_at_ms: 1 });
		feed({ type: "reasoning_delta", turn_id: "t9", id: "r9", reasoning: "R" });
		feed({ type: "text_delta", turn_id: "t9", text: "a" });
		feed({ type: "reasoning_delta", turn_id: "t9", id: "r9b", reasoning: "R2" });
		feed({ type: "text_delta", turn_id: "t9", text: "b" });
		await sleep(45);
		assert.deepStrictEqual(view.order, ["reasoning:r9", "text:t9", "reasoning:r9b", "text:t9"]);
		assert.strictEqual(view.assistantText.get("t9"), "ab");
		assert.strictEqual(view.reasoningText.get("t9:r9"), "R");
		assert.strictEqual(view.reasoningText.get("t9:r9b"), "R2");
	});

	test("steering: queued → drained by entry id; discard clears with a note", () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		feed({ type: "user_message", entry_id: "e0", text: "first" });
		feed({ type: "message_queued", id: "q1", text: "steer one" });
		feed({ type: "message_queued", id: "q2", text: "steer two" });
		assert.deepStrictEqual(view.pending.map(p => p.id), ["q1", "q2"]);
		feed({ type: "user_message", entry_id: "q1", text: "steer one" });
		assert.deepStrictEqual(view.pending.map(p => p.id), ["q2"]);
		feed({ type: "messages_discarded", messages: [{ id: "q2", text: "steer two" }] });
		assert.deepStrictEqual(view.pending, []);
		assert.strictEqual(view.notes.at(-1)?.kind, "warn");
	});

	test("turn_retried drops the turn's blocks and its buffered deltas", async () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		feed({ type: "turn_started", id: "t1", started_at_ms: 1 });
		feed({ type: "text_delta", turn_id: "t1", text: "draft" });
		feed({ type: "reasoning_delta", turn_id: "t1", id: "r1", reasoning: "hm" });
		feed({ type: "tool_call", turn_id: "t1", name: "bash", call_id: "c1", internal_call_id: "i1", arguments: null });
		feed({ type: "turn_retried", turn_id: "t1" });
		// The retried turn's still-buffered deltas must never paint.
		await sleep(45);
		assert.ok((view.removedTurns).includes("t1"));
		assert.strictEqual(view.assistantText.get("t1"), undefined);
		assert.strictEqual(view.reasoningText.get("t1:r1"), undefined);
		assert.strictEqual(view.tools.has("i1"), false);
	});

	test("the slash space: /compact rides the wire; /help lists; /exit quits; skills format the tag", () => {
		const { backend, view, mode, feed, control } = harness();
		boot(control, feed);
		let quit = 0;
		mode.onQuit = () => quit++;
		mode.setKeybindings([{ action: "interrupt", keys: ["escape", "ctrl+c"], description: "Interrupt the running turn" }]);
		feed({
			type: "skills_available",
			skills: [
				{ name: "code-quality-checklist", description: "A checklist for code quality", location: "l", level: "user" },
				{ name: "tests-quality-checklist", description: "A checklist for tests", location: "l2", level: "user" },
			],
		});
		assert.deepStrictEqual(view.skills.map(s => s.name), ["code-quality-checklist", "tests-quality-checklist"]);

		mode.submit("/compact");
		assert.deepStrictEqual(backend.sent, [{ kind: "compact", session: SESSION }]);

		mode.submit("/help");
		const info = view.notes.filter(n => n.kind === "info").map(n => n.text);
		assert.strictEqual(info.some(t => t.includes("/compact")), true);
		assert.strictEqual(info.some(t => t.includes("escape / ctrl+c")), true);

		mode.submit("/exit");
		mode.submit("/quit");
		assert.strictEqual(quit, 2);
		assert.strictEqual((backend.sent).length, 1); // quitting is local, never a wire frame

		mode.submit("/code-quality-checklist");
		// Skill invocation formats the wire's tag (the interim UX until
		// chips land) — a plain message carrying the marker.
		assert.deepStrictEqual(backend.sent[1], { kind: "message", session: SESSION, text: '<skill name="code-quality-checklist"/>' });
		// Trailing text rides along as the message (tags take no arguments).
		mode.submit("/code-quality-checklist focus on the diff");
		assert.deepStrictEqual(backend.sent[2], { kind: "message", session: SESSION, text: '<skill name="code-quality-checklist"/> focus on the diff' });

		mode.submit("/no-such-command");
		assert.strictEqual(view.notes.at(-1)?.kind, "warn");
		assert.ok((view.notes.at(-1)?.text ?? "").includes("not a command"));
		mode.submit("/compact focus on the auth module"); // v16: guidance rides as directives
		assert.deepStrictEqual(backend.sent[3], { kind: "compact", session: SESSION, directives: "focus on the auth module" });
		assert.strictEqual((view.notes.filter(n => n.kind === "warn")).length, 1);
	});

	test("the command table is the one home: the dropdown list and interpreter cannot diverge", () => {
		const { mode, feed, control } = harness();
		boot(control, feed);
		// Static commands first — each carries its behavior.
		const before = mode.slashCommands();
		assert.deepStrictEqual(before.map(c => c.name), ["compact", "help", "login", "logout", "model", "tree", "exit", "quit"]);
		assert.strictEqual(before.every(c => c.kind === "command"), true);

		// Skills join the same table as skill-typed entries (the dropdown's
		// type column reads the kind).
		feed({ type: "skills_available", skills: [{ name: "my-skill", description: "d", location: "l", level: "user" }] });
		const after = mode.slashCommands();
		assert.strictEqual((after).length, 9);
		assert.partialDeepStrictEqual(after.find(c => c.name === "my-skill"), { kind: "skill" });
	});

	test("skills fold per stream (v20): a child's catalog never clobbers the focused stream's, and follows focus", () => {
		const { view, mode, feed, control } = harness();
		boot(control, feed);
		feed({ type: "skills_available", skills: [{ name: "commit", description: "d", location: "l", level: "user" }] });
		assert.deepStrictEqual(view.skills.map(s => s.name), ["commit"]);

		// A subagent child announces itself, then its own catalog on its own
		// stamp — it must not touch the focused (root) stream's list.
		feed({ type: "session_opened", id: CHILD, path: "", cwd: "", model: { provider: "p", model: "m1" }, resumed: false, parent: SESSION }, CHILD);
		feed({ type: "skills_available", skills: [{ name: "child-skill", description: "d", location: "l", level: "user" }] }, CHILD);
		assert.deepStrictEqual(view.skills.map(s => s.name), ["commit"]);

		// Focus follows: the child's catalog shows while it is focused, the
		// root's returns on the walk back.
		mode.focusStream(CHILD);
		assert.deepStrictEqual(view.skills.map(s => s.name), ["child-skill"]);
		mode.focusStream(SESSION);
		assert.deepStrictEqual(view.skills.map(s => s.name), ["commit"]);

		// v20 announces only when discovery found something — absence is
		// unambiguous, so the next session clears until its catalog lands.
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		assert.deepStrictEqual(view.skills, []);
	});

	test("select_one cards answer exactly once, with the label; run terminals close leftovers", () => {
		const { backend, view, mode, feed, control } = harness();
		boot(control, feed);
		feed({ type: "user_message", entry_id: "e1", text: "go" });
		feed({
			type: "interaction_request",
			id: "ask1",
			ui_type: "native:select_one",
			payload: { title: "Allow bash?", body: "ls -la", options: [{ label: "Allow" }, { label: "Deny" }], free_text: true },
		});
		assert.strictEqual((view.cards).length, 1);
		assert.partialDeepStrictEqual(view.cards[0], { id: "ask1", options: ["Allow", "Deny"], freeText: true });

		mode.answerCard("ask1", ["Allow"], "needs an excluded path");
		assert.deepStrictEqual(backend.sent, [{ kind: "interaction_response", session: SESSION, id: "ask1", payload: { selected: ["Allow"], text: "needs an excluded path" } }]);
		assert.deepStrictEqual(view.closed, [{ id: "ask1", note: undefined }]);
		mode.answerCard("ask1", ["Allow"], null); // stale answer: no second send
		assert.strictEqual((backend.sent).length, 1);

		feed({ type: "interaction_request", id: "ask2", ui_type: "native:select_one", payload: { title: "t", body: "b", options: [{ label: "A" }] } });
		feed({ type: "run_finished", output: "", durable: true, started_at_ms: 1, completed_at_ms: 2 });
		assert.strictEqual(view.closed.some(c => c.id === "ask2" && c.note === "run finished"), true);
	});

	test("the model picker dispatch: /model routes to the root, select sends the command", () => {
		const { backend, mode, feed, control } = harness();
		boot(control, feed);
		let opened = 0;
		mode.onModel = () => opened++;
		mode.submit("/model");
		assert.strictEqual(opened, 1);
		assert.strictEqual(backend.sent.length, 0); // opening is local

		// The register tracks session_opened / model_changed for the ✓.
		assert.deepStrictEqual(mode.currentSelection, { provider: "p", model: "m1" });
		feed({ type: "model_changed", provider: "q", model: "m2", thinking_level: null });
		assert.deepStrictEqual(mode.currentSelection, { provider: "q", model: "m2" });

		mode.switchModel("anthropic", "claude-opus");
		assert.deepStrictEqual(backend.sent, [{ kind: "model", session: SESSION, provider: "anthropic", model: "claude-opus" }]);
	});

	test("the auth dispatch: /login and /logout route to the root, confirms send session-less commands", () => {
		const { backend, mode, feed, control } = harness();
		boot(control, feed);
		let loginOpened = 0;
		let logoutOpened = 0;
		mode.onLogin = () => loginOpened++;
		mode.onLogout = () => logoutOpened++;
		mode.submit("/login");
		mode.submit("/logout");
		assert.strictEqual(loginOpened, 1);
		assert.strictEqual(logoutOpened, 1);
		assert.strictEqual(backend.sent.length, 0); // opening is local

		// Backend-level commands: no session field, and they work even
		// before any selection exists (the zero-config fix path).
		mode.login("openai", "sk-test");
		mode.logout("openai");
		assert.deepStrictEqual(backend.sent, [
			{ kind: "login", provider: "openai", apiKey: "sk-test" },
			{ kind: "logout", provider: "openai" },
		]);
	});

	test("interaction_settled closes the card (v17); already-answered and unknown ids are no-ops", () => {
		const { view, mode, feed, control } = harness();
		boot(control, feed);
		feed({ type: "user_message", entry_id: "e1", text: "go" });
		feed({
			type: "interaction_request",
			id: "ask1",
			ui_type: "native:select_one",
			payload: { title: "Allow bash?", body: "ls", options: [{ label: "Allow" }, { label: "Deny" }] },
		});
		assert.strictEqual((view.cards).length, 1);

		// The settle close (answered elsewhere, retracted, dead channel):
		// the card drops long before any run terminal.
		feed({ type: "interaction_settled", id: "ask1" });
		assert.deepStrictEqual(view.closed, [{ id: "ask1", note: undefined }]);
		assert.strictEqual(mode.hasOpenCard, false);

		// A settle for an id we don't hold (e.g. answered here first) is a no-op.
		feed({ type: "interaction_settled", id: "ask1" });
		assert.strictEqual((view.closed).length, 1);
	});

	test("unknown card shapes render a cannot-answer notice, never an answer", () => {
		const { backend, view, feed, control } = harness();
		boot(control, feed);
		feed({ type: "interaction_request", id: "x1", ui_type: "ext:foo:widget", payload: { anything: true } });
		feed({ type: "interaction_request", id: "x2", ui_type: "native:select_any", payload: { title: "t", options: [] } });
		assert.strictEqual((view.cards).length, 0);
		assert.strictEqual((view.notes.filter(n => n.text.startsWith("cannot answer card"))).length, 2);
		assert.deepStrictEqual(backend.sent, []);
	});

	test("the replay pass renders through the same handlers without liveness", async () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		feed({ type: "replay_begin", total: 6 });
		feed({ type: "user_message", entry_id: "e1", text: "old" });
		assert.strictEqual(view.footer?.running ?? false, false); // suppressed inside brackets
		feed({ type: "turn_started", id: "t1", started_at_ms: 1 });
		feed({ type: "text_delta", turn_id: "t1", text: "archived answer" });
		feed({ type: "completion_call", turn_id: "t1", usage: { input_tokens: 9, output_tokens: 4, total_tokens: 13, cached_input_tokens: 0, cache_creation_input_tokens: 0 } });
		feed({ type: "replay_end" });
		await sleep(45);
		assert.strictEqual(view.replayBegun, 1);
		assert.strictEqual(view.replayEnded, 1);
		assert.strictEqual(view.assistantText.get("t1"), "archived answer");
		assert.strictEqual(view.footer?.inputTokens, 9); // history's usage rides the same handler
	});

	test("M2 focus: submit and session commands route to the focused stream", () => {
		const { backend, mode, feed, control } = harness();
		boot(control, feed);
		feed({ type: "session_opened", id: CHILD, path: "", cwd: "", model: { provider: "p", model: "m1" }, resumed: false, parent: SESSION }, CHILD);
		mode.focusStream(CHILD);
		assert.strictEqual(mode.focusedStream, CHILD);
		mode.submit("steer the child");
		mode.submit("/compact focus on the diff");
		mode.switchModel("p", "m2");
		assert.deepStrictEqual(
			backend.sent.map(f => `${f.kind}:${f.session}`),
			["message:0199bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "compact:0199bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "model:0199bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],
		);
	});

	test("M2 Esc's law: abort the focused run; idle child walks to its parent; idle root is a no-op", () => {
		const { backend, mode, feed, control } = harness();
		boot(control, feed);
		// Root idle: nothing happens, the key falls through.
		assert.strictEqual(mode.escape(), false);
		// Root running: abort the root.
		feed({ type: "user_message", entry_id: "e1", text: "go" });
		assert.strictEqual(mode.escape(), true);
		assert.deepStrictEqual(backend.sent.map(f => `${f.kind}:${f.session}`), [`abort:${SESSION}`]);
		feed({ type: "run_finished", output: "", durable: true, started_at_ms: 0, completed_at_ms: 1 });

		// Idle child focused: Esc walks to the parent, no abort sent.
		feed({ type: "session_opened", id: CHILD, path: "", cwd: "", model: { provider: "p", model: "m1" }, resumed: false, parent: SESSION }, CHILD);
		mode.focusStream(CHILD);
		assert.strictEqual(mode.escape(), true);
		assert.strictEqual(mode.focusedStream, SESSION);
		assert.strictEqual(backend.sent.length, 1); // still only the root's abort

		// Running child focused: Esc aborts THE CHILD, not the root.
		mode.focusStream(CHILD);
		feed({ type: "user_message", entry_id: "cx", text: "child task" }, CHILD);
		assert.strictEqual(mode.escape(), true);
		assert.deepStrictEqual(backend.sent.map(f => `${f.kind}:${f.session}`), [`abort:${SESSION}`, `abort:${CHILD}`]);
	});

	test("M2 the list projection: title from the parent call's task, the activity atom's priority", () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		// The parent's subagent tool call carries the task text.
		feed({ type: "user_message", entry_id: "e1", text: "delegate" });
		feed({ type: "turn_started", id: "t1", started_at_ms: 0 });
		feed({ type: "tool_call", turn_id: "t1", name: "subagent", call_id: "c1", internal_call_id: "i1", arguments: JSON.stringify({ task: "survey the diff landscape\nsecond line" }) });
		feed({ type: "session_opened", id: CHILD, path: "", cwd: "", model: { provider: "p", model: "m1" }, resumed: false, parent: SESSION, parent_call: "i1" }, CHILD);
		assert.deepStrictEqual(
			view.subagents.map(e => ({ stream: e.stream, title: e.title, state: e.state })),
			[{ stream: CHILD, title: "survey the diff landscape", state: "idle" }],
		);
		// running (the task message opens the run)…
		feed({ type: "user_message", entry_id: "cx", text: "survey the diff landscape" }, CHILD);
		assert.strictEqual(view.subagents[0]!.state, "running");
		// …thinking (a reasoning delta is the latest)…
		feed({ type: "turn_started", id: "ct1", started_at_ms: 1 }, CHILD);
		feed({ type: "reasoning_delta", turn_id: "ct1", id: "r1", reasoning: "hmm" }, CHILD);
		assert.strictEqual(view.subagents[0]!.state, "thinking");
		// …a tool call outranks thinking…
		feed({ type: "tool_call", turn_id: "ct1", name: "read", call_id: "c2", internal_call_id: "i2", arguments: "{}" }, CHILD);
		assert.strictEqual(view.subagents[0]!.state, "read");
		// …a card outranks everything…
		feed({ type: "interaction_request", id: "ask-c", ui_type: "native:select_one", payload: { title: "Allow?", body: "", options: [{ label: "yes" }] } }, CHILD);
		assert.strictEqual(view.subagents[0]!.state, "waiting");
		// …and the terminal settles to completed with the idle clock armed.
		feed({ type: "tool_result", turn_id: "ct1", entry_id: "e2", name: "read", internal_call_id: "i2", content: "x", status: { status: "success" } }, CHILD);
		feed({ type: "interaction_settled", id: "ask-c" }, CHILD);
		feed({ type: "run_finished", output: "", durable: true, started_at_ms: 1, completed_at_ms: 2 }, CHILD);
		assert.strictEqual(view.subagents[0]!.state, "completed");
		assert.strictEqual(typeof view.subagents[0]!.idleSince, "number");
	});

	test("M2 cards are view-independent: a child's card surfaces labeled and answers to its own stream", () => {
		const { backend, view, mode, feed, control } = harness();
		boot(control, feed);
		feed({ type: "user_message", entry_id: "e1", text: "go" });
		feed({ type: "turn_started", id: "t1", started_at_ms: 0 });
		feed({ type: "tool_call", turn_id: "t1", name: "subagent", call_id: "c1", internal_call_id: "i1", arguments: JSON.stringify({ task: "fix the parser" }) });
		feed({ type: "session_opened", id: CHILD, path: "", cwd: "", model: { provider: "p", model: "m1" }, resumed: false, parent: SESSION, parent_call: "i1" }, CHILD);
		// The root stays focused — the child's card still surfaces, labeled.
		feed({ type: "interaction_request", id: "ask-c", ui_type: "native:select_one", payload: { title: "Allow?", body: "", options: [{ label: "yes" }] } }, CHILD);
		assert.strictEqual(view.cards.length, 1);
		assert.strictEqual(view.cardLabels[0], "fix the parser");
		// The answer routes to the CARD's stream, wherever focus sits.
		mode.answerCard("ask-c", ["yes"], null);
		assert.deepStrictEqual(backend.sent, [{ kind: "interaction_response", session: CHILD, id: "ask-c", payload: { selected: ["yes"], text: null } }]);
	});

	test("M2 focus switch re-emits the dock from the focused stream; the footer names a focused child", () => {
		const { view, mode, feed, control } = harness();
		boot(control, feed);
		feed({ type: "session_opened", id: CHILD, path: "", cwd: "/child/dir", model: { provider: "p", model: "child-model" }, resumed: false, parent: SESSION }, CHILD);
		mode.focusStream(CHILD);
		assert.strictEqual(view.shownStreams.at(-1), CHILD);
		assert.strictEqual(view.footer?.streamLabel, CHILD); // no parent_call → title fallback is the id
		assert.strictEqual(view.footer?.cwd, "/child/dir");
		assert.strictEqual(view.footer?.model, "child-model");
		mode.focusStream(SESSION);
		assert.strictEqual(view.footer?.streamLabel, undefined);
	});

	test("child frames fold into the child's own stream — never the shown one (M2)", () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		const before = view.users.length;
		// Unannounced streams violate the contract: dropped, logged.
		feed({ type: "user_message", entry_id: "cx", text: "orphan" }, CHILD);
		assert.strictEqual(view.users.length, before);

		// Announced, the child folds on its own stamp: the pane records the
		// stream, the list projection gains the row, the footer's facts stay
		// the root's.
		feed({ type: "session_opened", id: CHILD, path: "", cwd: "", model: { provider: "p", model: "m1" }, resumed: false, parent: SESSION }, CHILD);
		feed({ type: "user_message", entry_id: "cx", text: "child prompt" }, CHILD);
		assert.strictEqual(view.users.length, before + 1);
		assert.strictEqual(view.userStreams.at(-1), CHILD);
		assert.deepStrictEqual(view.subagents.map(e => e.stream), [CHILD]);
		assert.strictEqual(view.footer?.session, SESSION);
	});

	test("unknown frame types surface as notes; the connection is kept", () => {
		const { mode, view } = harness();
		mode.handleFrame(parseServerFrame(JSON.stringify({ type: "future_thing", x: 1 }))!);
		assert.strictEqual(view.notes.some(n => n.text.includes("future_thing")), true);
	});

	test("a startup failure (pre-boot backend-level error) is fatal with the reason; later ones are notes", () => {
		const { mode, view, feed, control } = harness();
		let reason = "";
		mode.onFatal = r => {
			reason = r;
		};
		// v19: the startup-failure shape is the report, one unstamped error
		// carrying the reason, then a nonzero exit — display the reason.
		// (feed's default stamp would make it session traffic; handleFrame
		// directly to keep it backend-level.)
		mode.handleFrame({ kind: "event", stream: undefined, event: { type: "error", kind: "session", message: "no config — see the setup guide" } });
		assert.strictEqual(reason, "no config — see the setup guide");

		// Once the boot session is open, the same kind is an ordinary
		// backend-level error (an unknown-session command's outcome) — a
		// note, never a death.
		boot(control, feed);
		reason = "";
		mode.handleFrame({ kind: "event", stream: undefined, event: { type: "error", kind: "session", message: "no such session" } });
		assert.strictEqual(reason, "");
		assert.strictEqual(view.notes.some(n => n.kind === "error" && n.text.includes("no such session")), true);
	});

	test("a report naming any other protocol version is fatal, never limped on", () => {
		const { mode, control } = harness();
		let reason = "";
		mode.onFatal = r => {
			reason = r;
		};
		control({ type: "report", protocol_version: PROTOCOL_VERSION - 1 });
		assert.ok((reason).includes(`v${PROTOCOL_VERSION - 1}`));
		assert.ok((reason).includes(`v${PROTOCOL_VERSION}`));
		assert.strictEqual(mode.activeSession, undefined);
	});

	test("compaction envelope: steps meter like turns, end delivers the context length", () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		feed({ type: "model_changed", provider: "p", model: "m1", thinking_level: null, context_window: 200000 });
		feed({ type: "completion_call", turn_id: "t1", usage: { input_tokens: 900, output_tokens: 100, total_tokens: 1000, cached_input_tokens: 0, cache_creation_input_tokens: 0 }, cost: 0.001 });
		assert.strictEqual(view.footer?.contextUsed, 1000);

		feed({ type: "compaction_begin" });
		assert.strictEqual(view.status, "compacting context…");
		// Summarization spend meters exactly like a completion_call's (v15).
		feed({ type: "compaction_step", id: "c1", usage: { input_tokens: 800, output_tokens: 50, total_tokens: 850, cached_input_tokens: 0, cache_creation_input_tokens: 0 }, cost: 0.0005 });
		assert.strictEqual(view.footer?.inputTokens, 1700);
		costCloseTo(view.footer?.cost, 0.0015);
		feed({ type: "compaction_end", tokens_after: 4200 });
		assert.strictEqual(view.footer?.contextUsed, 4200); // authoritative post-compaction length
		assert.strictEqual(view.status, "idle");
		assert.strictEqual(view.notes.some(n => n.text.startsWith("context compacted")), true);

		// The next request's fresh total wins again from then on.
		feed({ type: "completion_call", turn_id: "t2", usage: { input_tokens: 10, output_tokens: 5, total_tokens: 4300, cached_input_tokens: 0, cache_creation_input_tokens: 0 } });
		assert.strictEqual(view.footer?.contextUsed, 4300);
	});

	test("compaction failure: error note, status restored, nothing metered", () => {
		const { view, feed, control } = harness();
		boot(control, feed);
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		feed({ type: "user_message", entry_id: "e1", text: "go" });
		feed({ type: "compaction_begin" });
		feed({ type: "compaction_failed", message: "provider down" });
		assert.strictEqual(view.notes.some(n => n.text.includes("compaction failed")), true);
		assert.strictEqual(view.status, "working — esc interrupts"); // the run continues
		assert.strictEqual(view.footer?.inputTokens, 0);
	});

	test("the model/provider catalogs (v21+v22): last-wins folds, the setup predicate warns with the branched fix", () => {
		const { view, mode, feed, control } = harness();
		boot(control, feed);
		feed({
			type: "models_available",
			providers: [
				{ id: "anthropic", models: [{ id: "claude", reasoning: true, input: ["text"], thinking_levels: ["low"] }] },
				{ id: "local", name: "Local", models: [{ id: "m1", reasoning: false, input: ["text"], thinking_levels: [] }] },
			],
		});
		feed({
			type: "providers_available",
			providers: [
				{ id: "anthropic", auth: "stored" },
				{ id: "local", auth: "keyless" },
				{ id: "openai", auth: "none" },
			],
		});
		assert.strictEqual(mode.modelsCatalog.length, 2);
		assert.strictEqual(mode.modelsCatalog[0]!.id, "anthropic");
		assert.deepStrictEqual(mode.providerStatuses.map(p => `${p.id}:${p.auth}`), ["anthropic:stored", "local:keyless", "openai:none"]);
		assert.ok(view.notes.some(n => n.text.includes("no key for: openai")));

		// Re-announcement replaces wholesale (login landed elsewhere).
		feed({ type: "models_available", providers: [{ id: "openai", models: [{ id: "gpt", reasoning: false, input: ["text"], thinking_levels: [] }] }] });
		feed({ type: "providers_available", providers: [{ id: "openai", auth: "stored" }] });
		assert.deepStrictEqual(mode.modelsCatalog.map(p => p.id), ["openai"]);
		assert.strictEqual(mode.providerStatuses.length, 1);

		// Empty catalog + key-less providers: config exists, nothing
		// usable — login fixes in-app.
		feed({ type: "models_available", providers: [] });
		feed({ type: "providers_available", providers: [{ id: "openai", auth: "none" }] });
		assert.strictEqual(view.notes.at(-1)?.kind, "warn");
		assert.ok((view.notes.at(-1)?.text ?? "").includes("missing keys for: openai"));

		// No providers at all: no config — the restart path.
		feed({ type: "models_available", providers: [] });
		feed({ type: "providers_available", providers: [] });
		assert.ok((view.notes.at(-1)?.text ?? "").includes("providers.toml"));
	});

	test("the zero-config boot (v21): session_opened model null, no model_changed, the teaching note warns", () => {
		const { view, feed, control } = harness();
		control({ type: "report", protocol_version: PROTOCOL_VERSION });
		feed({ type: "session_opened", id: SESSION, path: "", cwd: "", model: null, resumed: false });
		assert.strictEqual(view.footer?.model, undefined);
		assert.strictEqual(view.footer?.modelName, undefined);
		// The boot's teaching note rides error { kind: model } — a warning,
		// not an error (§6: the session runs selection-less).
		feed({ type: "error", kind: "model", message: "no usable model — log in or configure one" });
		assert.strictEqual(view.notes.at(-1)?.kind, "warn");
		// A selection-less run fails at open as run_failed, still an error.
		feed({ type: "user_message", entry_id: "e1", text: "hi" });
		feed({ type: "run_failed", kind: "model", message: "no model selected", started_at_ms: 1, completed_at_ms: 2 });
		assert.strictEqual(view.notes.at(-1)?.kind, "error");
		// The first model command lands the selection; facts arrive.
		feed({ type: "model_changed", provider: "p", model: "m1", thinking_level: null, context_window: 1000 });
		assert.strictEqual(view.footer?.model, "m1");
		assert.strictEqual(view.footer?.contextWindow, 1000);
	});

	test("the session tree: chain events feed it, checkout moves the head and rides the wire, /tree dispatches", () => {
		const { backend, mode, feed, control } = harness();
		boot(control, feed);
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		feed({ type: "user_message", entry_id: "e1", text: "go" });
		feed({ type: "turn_started", id: "t1", started_at_ms: 1 });
		feed({ type: "text_delta", turn_id: "t1", text: "Working." });
		feed({ type: "tool_call", turn_id: "t1", name: "bash", call_id: "c1", internal_call_id: "i1", arguments: "{\"cmd\":\"ls\"}" });
		feed({ type: "tool_result", turn_id: "t1", entry_id: "e2", name: "bash", internal_call_id: "i1", content: "ok", status: { status: "success" } });

		// The store built the chain; the tool row shows the call.
		assert.deepStrictEqual(mode.tree.rows().map(row => row.id), ["e1", "t1", "e2"]);
		assert.strictEqual(mode.tree.rows().find(row => row.id === "e2")!.preview, "bash cmd: ls");

		// Rewind via the tree: the command names the session and entry.
		mode.checkout("e1");
		assert.deepStrictEqual(backend.sent, [{ kind: "checkout", session: SESSION, entryId: "e1" }]);
		feed({ type: "checked_out", entry_id: "e1", base_id: null });
		assert.strictEqual(mode.tree.headId, "e1");
		// The next message branches off the rewound head.
		feed({ type: "user_message", entry_id: "e3", text: "again" });
		assert.deepStrictEqual(mode.tree.rows().map(row => row.id), ["e1", "e3", "t1", "e2"]);

		// /tree dispatches to the root's callback.
		let opened = 0;
		mode.onTree = () => opened++;
		mode.submit("/tree");
		assert.strictEqual(opened, 1);
		assert.partialDeepStrictEqual(mode.slashCommands().find(c => c.name === "tree"), { kind: "command" });

		// A fresh session_opened resets the tree with the session.
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		assert.strictEqual(mode.tree.size, 0);
	});
});
