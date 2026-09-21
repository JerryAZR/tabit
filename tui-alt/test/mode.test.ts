/**
 * The mode against a scripted frame feed — the spike's stub-context
 * pattern: hand-emit typed events, assert on the view recorder. No TUI
 * boots here; rendering is the engine's job, routing/state is the mode's.
 * The recorder logs every block mutation in order so the coalescer's
 * wire-order guarantee is assertable.
 */

import { describe, expect, test } from "bun:test";

import { InteractiveMode, type BackendLink, type FooterFacts, type InteractionCard, type ModeView, type PendingMessage, type SkillInfo } from "../src/mode";
import { parseServerFrame, type ParsedServerFrame } from "../src/protocol";

const SESSION = "0199aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CHILD = "0199bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

class FakeBackend implements BackendLink {
	readonly sent: Array<{ kind: string; session?: string; id?: string; payload?: unknown; text?: string }> = [];
	message(session: string, text: string): void {
		this.sent.push({ kind: "message", session, text });
	}
	abort(session: string): void {
		this.sent.push({ kind: "abort", session });
	}
	compact(session: string): void {
		this.sent.push({ kind: "compact", session });
	}
	interactionResponse(session: string, id: string, payload: unknown): void {
		this.sent.push({ kind: "interaction_response", session, id, payload });
	}
}

class RecordingView implements ModeView {
	replayBegun = 0;
	replayEnded = 0;
	users: Array<{ entryId: string; text: string }> = [];
	notes: Array<{ text: string; kind: string }> = [];
	assistantText = new Map<string, string>();
	reasoningText = new Map<string, string>();
	removedTurns: string[] = [];
	tools = new Map<string, { turnId: string; name: string; args: string | null; content?: string; ok?: boolean; details?: unknown }>();
	pending: PendingMessage[] = [];
	status = "";
	footer: FooterFacts | undefined;
	cards: InteractionCard[] = [];
	closed: Array<{ id: string; note: string | undefined }> = [];
	/** Every block mutation in apply order: `text:t1`, `reasoning:t1:r1`, `tool:i1`, `user:e1`. */
	order: string[] = [];

	beginReplay(): void {
		this.replayBegun++;
		this.assistantText.clear();
		this.reasoningText.clear();
		this.tools.clear();
	}
	endReplay(): void {
		this.replayEnded++;
	}
	addUser(entryId: string, text: string): void {
		this.users.push({ entryId, text });
		this.order.push(`user:${entryId}`);
	}
	addNote(text: string, kind: "info" | "warn" | "error"): void {
		this.notes.push({ text, kind });
	}
	appendAssistantText(turnId: string, text: string): void {
		this.assistantText.set(turnId, (this.assistantText.get(turnId) ?? "") + text);
		this.order.push(`text:${turnId}`);
	}
	appendReasoning(turnId: string, reasoningId: string, text: string): void {
		const key = `${turnId}:${reasoningId}`;
		this.reasoningText.set(key, (this.reasoningText.get(key) ?? "") + text);
		this.order.push(`reasoning:${reasoningId}`);
	}
	addTool(turnId: string, internalCallId: string, name: string, args: string | null): void {
		this.tools.set(internalCallId, { turnId, name, args });
		this.order.push(`tool:${internalCallId}`);
	}
	setToolResult(internalCallId: string, content: string, ok: boolean, details?: unknown): void {
		const tool = this.tools.get(internalCallId);
		if (tool !== undefined) {
			tool.content = content;
			tool.ok = ok;
			tool.details = details;
		}
	}
	removeTurn(turnId: string): void {
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
	showCard(card: InteractionCard): void {
		this.cards.push(card);
	}
	closeCard(id: string, note: string | undefined): void {
		this.closed.push({ id, note });
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

function ack(control: (frame: Extract<ParsedServerFrame, { kind: "control" }>["frame"]) => void, session = SESSION): void {
	control({ type: "initialize_ack", protocol_version: 15, session_id: session });
}

describe("InteractiveMode", () => {
	test("ack mints the routing key; submits address the active session", () => {
		const { backend, view, mode, control } = harness();
		expect(view.status).toBe("connecting…");
		control({ type: "initialize_ack", protocol_version: 15, session_id: SESSION });
		mode.submit("hello");
		expect(backend.sent).toEqual([{ kind: "message", session: SESSION, text: "hello" }]);
		// Pre-ack submits have no session to address — dropped, not sent.
		const fresh = harness();
		fresh.mode.submit("lost");
		expect(fresh.backend.sent).toEqual([]);
	});

	test("a live run: liveness on user_message, coalesced deltas, tool lifecycle, per-turn usage", async () => {
		const { view, mode, feed, control } = harness();
		ack(control);
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		feed({ type: "model_changed", provider: "p", model: "m1", thinking_level: null, context_window: 200000, name: "Model One", cost: { input: 1, output: 4, cache_read: 0.1, cache_write: 0.4 } });
		expect(view.footer?.path).toBe("/w"); // the editor's completion root
		feed({ type: "user_message", entry_id: "e1", text: "hi" });
		expect(view.footer?.running).toBe(true);
		expect(view.status).toBe("working — esc interrupts");
		expect(view.users).toEqual([{ entryId: "e1", text: "hi" }]);
		expect(view.footer?.modelName).toBe("Model One");
		expect(view.footer?.contextWindow).toBe(200000);
		expect(view.footer?.rates).toEqual({ input: 1, output: 4, cache_read: 0.1, cache_write: 0.4 });

		feed({ type: "turn_started", id: "t1", started_at_ms: 1 });
		feed({ type: "reasoning_delta", turn_id: "t1", id: "r1", reasoning: "think" });
		feed({ type: "text_delta", turn_id: "t1", text: "Hel" });
		feed({ type: "text_delta", turn_id: "t1", text: "lo" });
		await Bun.sleep(45); // flush timer (33 ms)
		expect(view.assistantText.get("t1")).toBe("Hello");
		expect(view.reasoningText.get("t1:r1")).toBe("think");

		feed({ type: "tool_call", turn_id: "t1", name: "bash", call_id: "c1", internal_call_id: "i1", arguments: "{\"cmd\":\"ls\"}" });
		expect(view.tools.get("i1")).toMatchObject({ name: "bash", args: "{\"cmd\":\"ls\"}" });
		feed({ type: "tool_result", turn_id: "t1", entry_id: "e2", name: "bash", internal_call_id: "i1", content: "ok", status: { status: "success" }, details: { trivial: true } });
		expect(view.tools.get("i1")).toMatchObject({ content: "ok", ok: true, details: { trivial: true } });

		// v12/v13: the per-turn report is the summing home — the run
		// terminal carries no usage at all.
		feed({ type: "completion_call", turn_id: "t1", usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15, cached_input_tokens: 3, cache_creation_input_tokens: 2 }, cost: 0.00003 });
		expect(view.footer?.inputTokens).toBe(10);
		expect(view.footer?.outputTokens).toBe(5);
		expect(view.footer?.cachedInputTokens).toBe(3);
		expect(view.footer?.cacheCreationTokens).toBe(2);
		expect(view.footer?.cost).toBeCloseTo(0.00003);

		feed({ type: "run_finished", output: "lo", durable: true, started_at_ms: 1, completed_at_ms: 2 });
		expect(view.footer?.running).toBe(false);
		expect(view.footer?.inputTokens).toBe(10);
		expect(mode.running).toBe(false);
	});

	test("the session log path fact: real path flows, ephemeral stays undefined", () => {
		const { view, feed, control } = harness();
		ack(control);
		feed({ type: "session_opened", id: SESSION, path: "C:\\proj\\.tabit\\s\\a.jsonl", model: { provider: "p", model: "m1" }, resumed: false });
		expect(view.footer?.path).toBe("C:\\proj\\.tabit\\s\\a.jsonl"); // the log file — session UI data, not a cwd

		const ephemeral = harness();
		ack(ephemeral.control);
		ephemeral.feed({ type: "session_opened", id: SESSION, path: "", model: { provider: "p", model: "m1" }, resumed: false });
		expect(ephemeral.view.footer?.path).toBeUndefined();
	});

	test("usage accounting: sums across turns and terminals, absent costs stay absent, replay re-sums after reset", async () => {
		const { view, feed, control } = harness();
		ack(control);
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
		expect(view.footer?.inputTokens).toBe(137);
		expect(view.footer?.outputTokens).toBe(27);
		expect(view.footer?.cachedInputTokens).toBe(50);
		expect(view.footer?.cacheCreationTokens).toBe(10);
		expect(view.footer?.cost).toBeCloseTo(0.0003); // absent cost skipped, not zeroed

		// A resume-style boot: facts reset, then the replay pass re-delivers
		// the history's completion_calls through the same handler.
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: true });
		expect(view.footer?.inputTokens).toBe(0);
		expect(view.footer?.cost).toBeUndefined();
		expect(view.footer?.resumed).toBe(true);
		feed({ type: "replay_started", total: 1 });
		feed({ type: "completion_call", turn_id: "t1", usage: usage(100, 20), cost: 0.0002 });
		feed({ type: "replay_done" });
		await Bun.sleep(45);
		expect(view.footer?.inputTokens).toBe(100);
		expect(view.footer?.outputTokens).toBe(20);
		expect(view.footer?.cost).toBeCloseTo(0.0002);
	});

	test("blocks appear in wire order: lazy creation, no pre-allocation at turn_started", async () => {
		const { view, feed, control } = harness();
		ack(control);
		feed({ type: "turn_started", id: "t9", started_at_ms: 1 });
		feed({ type: "reasoning_delta", turn_id: "t9", id: "r9", reasoning: "R" });
		feed({ type: "text_delta", turn_id: "t9", text: "a" });
		feed({ type: "reasoning_delta", turn_id: "t9", id: "r9b", reasoning: "R2" });
		feed({ type: "text_delta", turn_id: "t9", text: "b" });
		await Bun.sleep(45);
		expect(view.order).toEqual(["reasoning:r9", "text:t9", "reasoning:r9b", "text:t9"]);
		expect(view.assistantText.get("t9")).toBe("ab");
		expect(view.reasoningText.get("t9:r9")).toBe("R");
		expect(view.reasoningText.get("t9:r9b")).toBe("R2");
	});

	test("steering: queued → drained by entry id; discard clears with a note", () => {
		const { view, feed, control } = harness();
		ack(control);
		feed({ type: "user_message", entry_id: "e0", text: "first" });
		feed({ type: "message_queued", id: "q1", text: "steer one" });
		feed({ type: "message_queued", id: "q2", text: "steer two" });
		expect(view.pending.map(p => p.id)).toEqual(["q1", "q2"]);
		feed({ type: "user_message", entry_id: "q1", text: "steer one" });
		expect(view.pending.map(p => p.id)).toEqual(["q2"]);
		feed({ type: "messages_discarded", messages: [{ id: "q2", text: "steer two" }] });
		expect(view.pending).toEqual([]);
		expect(view.notes.at(-1)?.kind).toBe("warn");
	});

	test("turn_retried drops the turn's blocks and its buffered deltas", async () => {
		const { view, feed, control } = harness();
		ack(control);
		feed({ type: "turn_started", id: "t1", started_at_ms: 1 });
		feed({ type: "text_delta", turn_id: "t1", text: "draft" });
		feed({ type: "reasoning_delta", turn_id: "t1", id: "r1", reasoning: "hm" });
		feed({ type: "tool_call", turn_id: "t1", name: "bash", call_id: "c1", internal_call_id: "i1", arguments: null });
		feed({ type: "turn_retried", turn_id: "t1" });
		// The retried turn's still-buffered deltas must never paint.
		await Bun.sleep(45);
		expect(view.removedTurns).toContain("t1");
		expect(view.assistantText.get("t1")).toBeUndefined();
		expect(view.reasoningText.get("t1:r1")).toBeUndefined();
		expect(view.tools.has("i1")).toBe(false);
	});

	test("the slash space: /compact rides the wire; /exit quits; skills never send", () => {
		const { backend, view, mode, feed, control } = harness();
		ack(control);
		let quit = 0;
		mode.onQuit = () => quit++;
		feed({
			type: "skills_available",
			skills: [
				{ name: "code-quality-checklist", description: "A checklist for code quality", location: "l", level: "user" },
				{ name: "tests-quality-checklist", description: "A checklist for tests", location: "l2", level: "user" },
			],
		});
		expect(view.skills.map(s => s.name)).toEqual(["code-quality-checklist", "tests-quality-checklist"]);

		mode.submit("/compact");
		expect(backend.sent).toEqual([{ kind: "compact", session: SESSION }]);

		mode.submit("/exit");
		mode.submit("/quit");
		expect(quit).toBe(2);
		expect(backend.sent).toHaveLength(1); // quitting is local, never a wire frame

		mode.submit("/code-quality-checklist");
		expect(backend.sent).toHaveLength(1); // no wire invocation for skills
		expect(view.notes.at(-1)?.kind).toBe("warn");
		expect(view.notes.at(-1)?.text).toContain("not invocable");

		mode.submit("/no-such-command");
		mode.submit("/compact extra"); // arguments are not the bare command
		expect(view.notes.filter(n => n.kind === "warn")).toHaveLength(3);
	});

	test("select_one cards answer exactly once, with the label; run terminals close leftovers", () => {
		const { backend, view, mode, feed, control } = harness();
		ack(control);
		feed({ type: "user_message", entry_id: "e1", text: "go" });
		feed({
			type: "interaction_request",
			id: "ask1",
			ui_type: "native:select_one",
			payload: { title: "Allow bash?", body: "ls -la", options: [{ label: "Allow" }, { label: "Deny" }], free_text: true },
		});
		expect(view.cards).toHaveLength(1);
		expect(view.cards[0]).toMatchObject({ id: "ask1", options: ["Allow", "Deny"], freeText: true });

		mode.answerCard("ask1", 0);
		expect(backend.sent).toEqual([{ kind: "interaction_response", session: SESSION, id: "ask1", payload: { selected: ["Allow"], text: null } }]);
		expect(view.closed).toEqual([{ id: "ask1", note: undefined }]);
		mode.answerCard("ask1", 0); // stale answer: no second send
		expect(backend.sent).toHaveLength(1);

		feed({ type: "interaction_request", id: "ask2", ui_type: "native:select_one", payload: { title: "t", body: "b", options: [{ label: "A" }] } });
		feed({ type: "run_finished", output: "", durable: true, started_at_ms: 1, completed_at_ms: 2 });
		expect(view.closed.some(c => c.id === "ask2" && c.note === "run finished")).toBe(true);
	});

	test("unknown card shapes render a cannot-answer notice, never an answer", () => {
		const { backend, view, feed, control } = harness();
		ack(control);
		feed({ type: "interaction_request", id: "x1", ui_type: "ext:foo:widget", payload: { anything: true } });
		feed({ type: "interaction_request", id: "x2", ui_type: "native:select_any", payload: { title: "t", options: [] } });
		expect(view.cards).toHaveLength(0);
		expect(view.notes.filter(n => n.text.startsWith("cannot answer card"))).toHaveLength(2);
		expect(backend.sent).toEqual([]);
	});

	test("the replay pass renders through the same handlers without liveness", async () => {
		const { view, feed, control } = harness();
		ack(control);
		feed({ type: "replay_started", total: 6 });
		feed({ type: "user_message", entry_id: "e1", text: "old" });
		expect(view.footer?.running ?? false).toBe(false); // suppressed inside brackets
		feed({ type: "turn_started", id: "t1", started_at_ms: 1 });
		feed({ type: "text_delta", turn_id: "t1", text: "archived answer" });
		feed({ type: "completion_call", turn_id: "t1", usage: { input_tokens: 9, output_tokens: 4, total_tokens: 13, cached_input_tokens: 0, cache_creation_input_tokens: 0 } });
		feed({ type: "replay_done" });
		await Bun.sleep(45);
		expect(view.replayBegun).toBe(1);
		expect(view.replayEnded).toBe(1);
		expect(view.assistantText.get("t1")).toBe("archived answer");
		expect(view.footer?.inputTokens).toBe(9); // history's usage rides the same handler
	});

	test("child-stream frames log without view noise", () => {
		const { view, feed, control } = harness();
		ack(control);
		const before = view.users.length;
		feed({ type: "user_message", entry_id: "cx", text: "child prompt" }, CHILD);
		expect(view.users.length).toBe(before);
	});

	test("unknown frame types surface as notes; the connection is kept", () => {
		const { mode, view } = harness();
		mode.handleFrame(parseServerFrame(JSON.stringify({ type: "future_thing", x: 1 }))!);
		expect(view.notes.some(n => n.text.includes("future_thing"))).toBe(true);
	});

	test("initialize_rejected is fatal with the backend's reason", () => {
		const { mode, control } = harness();
		let reason = "";
		mode.onFatal = r => {
			reason = r;
		};
		control({ type: "initialize_rejected", reason: "no config" });
		expect(reason).toBe("no config");
	});

	test("an ack from any other protocol version is fatal, never limped on", () => {
		const { mode, control } = harness();
		let reason = "";
		mode.onFatal = r => {
			reason = r;
		};
		control({ type: "initialize_ack", protocol_version: 14, session_id: SESSION });
		expect(reason).toContain("v14");
		expect(reason).toContain("v15");
		expect(mode.activeSession).toBeUndefined();
	});

	test("compaction envelope: steps meter like turns, end delivers the context length", () => {
		const { view, feed, control } = harness();
		ack(control);
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		feed({ type: "model_changed", provider: "p", model: "m1", thinking_level: null, context_window: 200000 });
		feed({ type: "completion_call", turn_id: "t1", usage: { input_tokens: 900, output_tokens: 100, total_tokens: 1000, cached_input_tokens: 0, cache_creation_input_tokens: 0 }, cost: 0.001 });
		expect(view.footer?.contextUsed).toBe(1000);

		feed({ type: "compaction_begin" });
		expect(view.status).toBe("compacting context…");
		// Summarization spend meters exactly like a completion_call's (v15).
		feed({ type: "compaction_step", id: "c1", usage: { input_tokens: 800, output_tokens: 50, total_tokens: 850, cached_input_tokens: 0, cache_creation_input_tokens: 0 }, cost: 0.0005 });
		expect(view.footer?.inputTokens).toBe(1700);
		expect(view.footer?.cost).toBeCloseTo(0.0015);
		feed({ type: "compaction_end", tokens_after: 4200 });
		expect(view.footer?.contextUsed).toBe(4200); // authoritative post-compaction length
		expect(view.status).toBe("idle");
		expect(view.notes.some(n => n.text.startsWith("context compacted"))).toBe(true);

		// The next request's fresh total wins again from then on.
		feed({ type: "completion_call", turn_id: "t2", usage: { input_tokens: 10, output_tokens: 5, total_tokens: 4300, cached_input_tokens: 0, cache_creation_input_tokens: 0 } });
		expect(view.footer?.contextUsed).toBe(4300);
	});

	test("compaction failure: error note, status restored, nothing metered", () => {
		const { view, feed, control } = harness();
		ack(control);
		feed({ type: "session_opened", id: SESSION, path: "/w", model: { provider: "p", model: "m1" }, resumed: false });
		feed({ type: "user_message", entry_id: "e1", text: "go" });
		feed({ type: "compaction_begin" });
		feed({ type: "compaction_failed", message: "provider down" });
		expect(view.notes.some(n => n.text.includes("compaction failed"))).toBe(true);
		expect(view.status).toBe("working — esc interrupts"); // the run continues
		expect(view.footer?.inputTokens).toBe(0);
	});
});
