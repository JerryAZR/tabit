/**
 * The subagent list widget, headless: visibility policy (running always,
 * idle within the hide window), selection movement, and the Enter intent.
 * The 60s hide is tested by feeding an already-expired `idleSince` — no
 * timer waits.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { IDLE_HIDE_MS, SubagentList } from "../src/components/subagent-list.ts";
import type { SubagentEntry } from "../src/mode.ts";

const entry = (stream: string, over: Partial<SubagentEntry> = {}): SubagentEntry => ({
	stream,
	parent: "root",
	title: stream,
	state: "running",
	running: true,
	idleSince: undefined,
	...over,
});

function harness() {
	const focused: string[] = [];
	const emptied: number[] = [];
	const list = new SubagentList({
		onFocus: stream => focused.push(stream),
		onEmpty: () => emptied.push(1),
		requestRender: () => {},
	});
	return { list, focused, emptied };
}

describe("subagent list widget", () => {
	test("running entries always show; idle entries hide past the window", () => {
		const { list } = harness();
		list.setEntries([
			entry("running-1"),
			entry("idle-fresh", { running: false, state: "idle", idleSince: Date.now() }),
			entry("idle-stale", { running: false, state: "idle", idleSince: Date.now() - IDLE_HIDE_MS - 1000 }),
		]);
		const rendered = list.render(80).join("\n");
		assert.ok(rendered.includes("running-1"));
		assert.ok(rendered.includes("idle-fresh"));
		assert.ok(!rendered.includes("idle-stale"));
		assert.strictEqual(list.isEmpty, false);
	});

	test("empty renders nothing and reads empty (the region law's 'when present')", () => {
		const { list } = harness();
		assert.deepStrictEqual(list.render(80), []);
		assert.strictEqual(list.isEmpty, true);
		// An all-expired set is equally absent.
		list.setEntries([entry("old", { running: false, idleSince: Date.now() - IDLE_HIDE_MS - 1 })]);
		assert.strictEqual(list.isEmpty, true);
	});

	test("arrows move the selection, Enter raises the focus intent", () => {
		const { list, focused } = harness();
		list.setEntries([entry("a"), entry("b")]);
		list.active = true;
		list.handleInput("\u001b[B"); // down
		list.handleInput("\u001b[B"); // clamps at the last row
		list.handleInput("\r"); // enter
		assert.deepStrictEqual(focused, ["b"]);
		list.handleInput("\u001b[A"); // up
		list.handleInput("\r");
		assert.deepStrictEqual(focused, ["b", "a"]);
	});

	test("re-projection keeps the cursor on its entry across reorderings", () => {
		const { list, focused } = harness();
		list.setEntries([entry("a"), entry("b")]);
		list.active = true;
		list.handleInput("\u001b[B"); // select b
		list.setEntries([entry("b"), entry("a")]); // reordered
		list.handleInput("\r");
		assert.deepStrictEqual(focused, ["b"]);
	});

	test("an emptied active list raises onEmpty (the vanished-region law)", () => {
		const { list, emptied } = harness();
		list.active = true;
		list.setEntries([]);
		assert.strictEqual(emptied.length, 1);
	});
});
