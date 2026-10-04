/**
 * The `/model` picker, headless: the pure rows builder (wire order, the
 * current register pinned, fallbacks, the compact context column), the
 * fuzzy filter's ranking, and the view's key model (↑/↓ wrap, typing
 * re-filters and re-homes, enter selects, esc closes) plus the column
 * alignment pin.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { buildModelRows, compactTokens, filterModelRows, ModelPickerView, type ModelPickerHooks } from "../src/model-picker.ts";
import { applyKeybindings } from "../src/keybindings.ts";
import type { AvailableProvider } from "../src/protocol.ts";

// The view resolves select actions through the global registry — installed
// in production by root.bind. Install the defaults here.
applyKeybindings();

const UP = "\x1b[A";
const DOWN = "\x1b[B";
const ENTER = "\r";
const ESCAPE = "\x1b";
const ANSI = /\x1b\[[0-9;]*m/g;

const model = (id: string, extra: Record<string, unknown> = {}) => ({
	reasoning: false,
	input: ["text"],
	thinking_levels: [],
	...extra,
	id,
});

const catalog: AvailableProvider[] = [
	{
		id: "anthropic",
		models: [
			model("claude-opus", { name: "Claude Opus", context_window: 200000 }),
			model("claude-haiku", { context_window: 200000 }),
		],
	},
	{
		id: "local",
		name: "Local Box",
		models: [model("qwen3-8b", { context_window: 32768 }), model("tiny")],
	},
];

describe("compactTokens", () => {
	test("blank when unstated; compact above 1000", () => {
		assert.strictEqual(compactTokens(undefined), "");
		assert.strictEqual(compactTokens(999), "999");
		assert.strictEqual(compactTokens(8192), "8.2k");
		assert.strictEqual(compactTokens(200000), "200k");
		assert.strictEqual(compactTokens(1_000_000), "1M");
	});
});

describe("buildModelRows", () => {
	test("wire order, fallbacks, and the current register pinned first", () => {
		const rows = buildModelRows(catalog, { provider: "local", model: "qwen3-8b" });
		assert.deepStrictEqual(
			rows.map(row => `${row.provider}/${row.model}`),
			["local/qwen3-8b", "anthropic/claude-opus", "anthropic/claude-haiku", "local/tiny"],
		);
		assert.strictEqual(rows[0]!.current, true);
		assert.strictEqual(rows[1]!.current, false);
		// Display fallbacks: model name ?? id, provider name ?? id.
		assert.strictEqual(rows[1]!.name, "Claude Opus");
		assert.strictEqual(rows[2]!.name, "claude-haiku");
		assert.strictEqual(rows[1]!.providerName, "anthropic");
		assert.strictEqual(rows[0]!.providerName, "Local Box");
		// The context column: compact when stated, blank when not (never zero).
		assert.strictEqual(rows[0]!.contextWindow, "32.8k");
		assert.strictEqual(rows[3]!.contextWindow, "");
	});

	test("a register outside the catalog marks nothing (the footer carries it)", () => {
		const rows = buildModelRows(catalog, { provider: "gone", model: "stale" });
		assert.strictEqual(rows.some(row => row.current), false);
		assert.strictEqual(rows[0]!.model, "claude-opus"); // wire order unpinned
	});

	test("no register (the zero-config boot) marks nothing", () => {
		assert.strictEqual(buildModelRows(catalog, undefined).some(row => row.current), false);
	});
});

describe("filterModelRows", () => {
	test("empty query keeps the build order; fuzzy narrows; provider/id ranks first", () => {
		const rows = buildModelRows(catalog, undefined);
		assert.deepStrictEqual(filterModelRows(rows, ""), rows);

		const narrowed = filterModelRows(rows, "claude");
		assert.deepStrictEqual(narrowed.map(row => row.model), ["claude-opus", "claude-haiku"]);

		// pi's ranking: the exact provider/id query beats bare-id noise.
		const exact = filterModelRows(rows, "local/tiny");
		assert.strictEqual(exact[0]!.model, "tiny");
		assert.strictEqual(exact[0]!.provider, "local");
	});
});

describe("ModelPickerView", () => {
	function rigUp(current?: { provider: string; model: string }) {
		const events: Array<{ kind: string; provider?: string; model?: string }> = [];
		const hooks: ModelPickerHooks = {
			onSelect: (provider, model) => events.push({ kind: "select", provider, model }),
			onClose: () => events.push({ kind: "close" }),
		};
		const view = new ModelPickerView(catalog, current, hooks, () => {});
		return { view, events };
	}

	test("↑/↓ wrap, enter selects the row's address, esc closes", () => {
		const { view, events } = rigUp();
		view.handleInput(UP); // wraps to the last row
		view.handleInput(ENTER);
		assert.deepStrictEqual(events[0], { kind: "select", provider: "local", model: "tiny" });
		view.handleInput(DOWN); // would wrap to the first
		const closed = rigUp();
		closed.view.handleInput(ESCAPE);
		assert.deepStrictEqual(closed.events, [{ kind: "close" }]);
	});

	test("typing re-filters and re-homes the cursor", () => {
		const { view, events } = rigUp();
		for (const char of "haiku") view.handleInput(char);
		view.handleInput(ENTER);
		assert.deepStrictEqual(events, [{ kind: "select", provider: "anthropic", model: "claude-haiku" }]);
	});

	test("columns align across the filtered set; the current row wears ✓", () => {
		const { view } = rigUp({ provider: "anthropic", model: "claude-opus" });
		const lines = view.render(80).map(line => line.replace(ANSI, ""));
		const opus = lines.find(line => line.includes("Claude Opus"))!;
		const qwen = lines.find(line => line.includes("qwen3-8b"))!;
		assert.ok(opus.includes("✓"));
		assert.ok(!qwen.includes("✓"));
		// The context column is right-aligned: the ENDS share a cell.
		assert.strictEqual(opus.indexOf("200k") + 4, qwen.indexOf("32.8k") + 5);
		// And the provider column starts at the same cell.
		assert.strictEqual(opus.indexOf("anthropic"), qwen.indexOf("Local Box"));
	});

	test("a filter with no matches renders the empty state, never a crash", () => {
		const { view, events } = rigUp();
		for (const char of "zzzz") view.handleInput(char);
		const lines = view.render(60).map(line => line.replace(ANSI, ""));
		assert.ok(lines.some(line => line.includes("no matches")));
		view.handleInput(ENTER);
		assert.deepStrictEqual(events, []); // nothing to select
	});
});
