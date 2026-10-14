/**
 * The thinking picker: the rows builder (default first, current pinned
 * and marked) and the view's navigation/select/close law.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { buildThinkingRows, ThinkingPickerView } from "../src/thinking-picker.ts";
import { applyKeybindings } from "../src/keybindings.ts";

// The view resolves select actions through the global registry — installed
// in production by root.bind. Install the defaults here.
applyKeybindings();

const UP = "\x1b[A";
const DOWN = "\x1b[B";
const ENTER = "\r";
const ESCAPE = "\x1b";

describe("buildThinkingRows", () => {
	test("default first, levels in wire order; current pinned first with the ✓", () => {
		const rows = buildThinkingRows(["low", "high"], undefined);
		assert.deepStrictEqual(rows.map(r => r.label), ["default", "low", "high"]);
		assert.ok(rows.every(r => !r.current)); // unannounced marks nothing

		const atDefault = buildThinkingRows(["low", "high"], null);
		assert.strictEqual(atDefault[0]!.current, true); // null marks the default row

		const atHigh = buildThinkingRows(["low", "high"], "high");
		assert.deepStrictEqual(atHigh.map(r => r.label), ["high", "default", "low"]); // pinned first
		assert.strictEqual(atHigh[0]!.current, true);
	});
});

describe("ThinkingPickerView", () => {
	test("navigation wraps, enter selects the level (or null for default), esc closes", () => {
		const selected: Array<string | null> = [];
		let closed = 0;
		const view = new ThinkingPickerView(
			["low", "high"],
			null,
			{ onSelect: level => selected.push(level), onClose: () => closed++ },
			() => {},
		);
		const rendered = view.render(60).join("\n");
		assert.ok(rendered.includes("thinking level"));
		assert.ok(rendered.includes("› default ✓")); // cursor starts on the current row
		assert.ok(rendered.includes("esc closes"));

		view.handleInput("\x1b[B"); // down → low
		view.handleInput("\r"); // enter
		assert.deepStrictEqual(selected, ["low"]);

		view.handleInput("\x1b[A"); // up → default (cursor state persists)
		view.handleInput("\r");
		assert.deepStrictEqual(selected, ["low", null]);

		view.handleInput("\x1b[B"); // down → low
		view.handleInput("\x1b[B"); // down → high
		view.handleInput("\x1b[B"); // wraps → default
		view.handleInput("\r");
		assert.deepStrictEqual(selected, ["low", null, null]);

		view.handleInput("\x1b"); // esc
		assert.strictEqual(closed, 1);
	});
});
