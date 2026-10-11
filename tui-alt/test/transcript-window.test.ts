/**
 * The alt-root against a fake Terminal — the render-level contract the
 * mode tests can't see: once the transcript exceeds the viewport, the
 * window must FOLLOW THE END and live content must keep painting.
 *
 * Regression: 2026-10-10 — the transcript slot was a plain Container,
 * hiding the pane's ScrollView from the layout engine (windowing only
 * engages for layout nodes), so any content past one screenful pinned
 * the view to the document top and live output never painted. Also
 * covered: the continue-run shape (turn_started with NO user_message)
 * lights the working indicator.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { setTimeout as sleep } from "node:timers/promises";
import type { Terminal } from "@earendil-works/pi-tui";

import { AltRoot } from "../src/root.ts";
import { InteractiveMode, type BackendLink } from "../src/mode.ts";
import type { ParsedServerFrame } from "../src/protocol.ts";

/** A capture-only terminal: writes accumulate for assertions. */
class FakeTerminal implements Terminal {
	readonly writes: string[] = [];
	start(_onInput: (data: string) => void, _onResize: () => void): void {}
	stop(): void {}
	async drainInput(): Promise<void> {}
	write(data: string): void {
		this.writes.push(data);
	}
	get columns(): number {
		return 100;
	}
	get rows(): number {
		return 40;
	}
	get kittyProtocolActive(): boolean {
		return false;
	}
	moveBy(): void {}
	hideCursor(): void {}
	showCursor(): void {}
	clearLine(): void {}
	clearFromCursor(): void {}
	clearScreen(): void {}
	setTitle(): void {}
	setProgress(): void {}
	setProgramStatus(): void {}
}

/** The painted screen (the alt screen's diff state) with ANSI stripped. */
function paintedScreen(root: AltRoot): string {
	const screen = (root.tui as unknown as { previousScreen: string[] }).previousScreen;
	return screen.map(row => row.replace(/\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07/g, "")).join("\n");
}

const SESSION = "s1";
const fakeBackend = new Proxy({}, { get: () => () => {} }) as unknown as BackendLink;

function harness() {
	const terminal = new FakeTerminal();
	const root = new AltRoot(terminal);
	const mode = new InteractiveMode(fakeBackend, root);
	root.bind(mode, () => {});
	root.tui.start();
	const feed = (event: Record<string, unknown>, stream: string | undefined = SESSION): void => {
		mode.handleFrame({ kind: "event", stream, event } as ParsedServerFrame);
	};
	return { terminal, root, mode, feed };
}

/** A replayed history tall enough to overflow the 40-row screen. */
function feedTallReplay(feed: (event: Record<string, unknown>) => void): void {
	feed({ type: "session_opened", id: SESSION, path: "", cwd: "", resumed: true });
	feed({ type: "replay_begin", total: 8 });
	feed({ type: "user_message", entry_id: "e0", text: "the top of history" });
	for (let i = 0; i < 8; i++) {
		feed({ type: "turn_started", id: `h${i}`, started_at_ms: 1 });
		feed({ type: "text_delta", turn_id: `h${i}`, text: `history turn ${i}\n\n${"- filler\n".repeat(10)}` });
		feed({ type: "tool_call", turn_id: `h${i}`, name: "read", call_id: `ch${i}`, internal_call_id: `ih${i}`, arguments: JSON.stringify({ path: `src/file${i}.rs` }) });
		feed({ type: "tool_result", turn_id: `h${i}`, entry_id: `eh${i}`, name: "read", internal_call_id: `ih${i}`, content: "x", status: { status: "success" } });
		feed({ type: "turn_committed", id: `h${i}`, completed_at_ms: 2 });
	}
	feed({ type: "replay_end" });
}

describe("the alt-root transcript window", () => {
	test("a tall replay follows the end, and live content keeps painting", async () => {
		const { root, feed } = harness();
		feedTallReplay(feed);
		await sleep(120);
		const afterReplay = paintedScreen(root);
		// The window sits at the END of the over-tall document: the last
		// turn visible, the first scrolled off.
		assert.ok(afterReplay.includes("src/file7.rs"), "the tail of the replay paints");
		assert.ok(!afterReplay.includes("the top of history"), "the top scrolled off");

		// The continue-run shape: a bare turn, no user_message.
		feed({ type: "turn_started", id: "t2", started_at_ms: 3 });
		feed({ type: "reasoning_delta", turn_id: "t2", id: "r0", reasoning: "thinking hard" });
		feed({ type: "text_delta", turn_id: "t2", text: "live answer" });
		feed({ type: "tool_call", turn_id: "t2", name: "bash", call_id: "c2", internal_call_id: "i2", arguments: "{}" });
		await sleep(200);
		const afterLive = paintedScreen(root);
		assert.ok(afterLive.includes("live answer"), "live text paints past a full window");
		assert.ok(afterLive.includes("bash"), "live tool block paints");
		assert.ok(afterLive.includes("working"), "the bare turn lights the working indicator");
		assert.ok(!afterLive.includes("the top of history"), "the window followed the end");
		root.dispose();
	});
});
