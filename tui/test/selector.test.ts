/**
 * The shared SelectorView's own contract — the behavior every consumer
 * inherits, tested once instead of per picker: wrap/page navigation, the
 * filtered set's counter, the title chrome, and the empty state. The
 * consumers' own tests cover their rows and continuations.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { SelectorView } from "../src/components/selector.ts";
import { applyKeybindings } from "../src/keybindings.ts";

applyKeybindings();

const UP = "\x1b[A";
const DOWN = "\x1b[B";
const LEFT = "\x1b[D";
const RIGHT = "\x1b[C";
const ENTER = "\r";
const ESCAPE = "\x1b";
const ANSI = /\x1b\[[0-9;]*m/g;

interface Row {
	id: string;
}

const rows = (n: number): Row[] => Array.from({ length: n }, (_, i) => ({ id: `row-${i + 1}` }));

function rigUp(entries: Row[], opts: { filter?: boolean; pageSize?: number; title?: string } = {}) {
	const selected: string[] = [];
	let closed = 0;
	const view = new SelectorView<Row, void>(
		{
			entries,
			...(opts.title !== undefined ? { title: opts.title } : {}),
			...(opts.pageSize !== undefined ? { pageSize: opts.pageSize } : {}),
			...(opts.filter === true ? { filter: { placeholder: "filter…", searchText: (row: Row) => row.id } } : {}),
			row: row => row.id,
		},
		{ onSelect: row => selected.push(row.id), onClose: () => closed++ },
		() => {},
	);
	return { view, selected, closed: () => closed, lines: () => view.render(60).map(line => line.replace(ANSI, "")) };
}

describe("the shared selector", () => {
	test("↑/↓ wrap; enter selects; esc closes", () => {
		const { view, selected, closed } = rigUp(rows(3));
		view.handleInput(UP); // wraps to the last
		view.handleInput(ENTER);
		assert.deepStrictEqual(selected, ["row-3"]);
		view.handleInput(DOWN); // wraps back to the first
		view.handleInput(ENTER);
		assert.deepStrictEqual(selected, ["row-3", "row-1"]);
		view.handleInput(ESCAPE);
		assert.strictEqual(closed(), 1);
	});

	test("←/→ page-clamp inside the viewport", () => {
		const { view, selected, lines } = rigUp(rows(30), { pageSize: 10 });
		assert.ok(!lines().some(line => line.includes("row-20"))); // off-page
		view.handleInput(RIGHT); // cursor 0 → 10
		view.handleInput(ENTER);
		assert.deepStrictEqual(selected, ["row-11"]);
		view.handleInput(LEFT); // back to 0
		view.handleInput(ENTER);
		assert.deepStrictEqual(selected, ["row-11", "row-1"]);
		view.handleInput(LEFT); // clamps at 0, never negative
		view.handleInput(ENTER);
		assert.deepStrictEqual(selected, ["row-11", "row-1", "row-1"]);
	});

	test("the counter is a catalog signal: present with a filter, absent without", () => {
		assert.ok(rigUp(rows(3), { filter: true }).lines().some(line => line.includes("(1/3)")));
		assert.ok(!rigUp(rows(3)).lines().some(line => line.includes("(1/3)")));
	});

	test("the title rides the chrome when stated", () => {
		assert.ok(rigUp(rows(2), { title: " thinking level" }).lines()[1]!.includes("thinking level"));
	});

	test("typing re-filters and re-homes; a dead query shows the empty state", () => {
		const { view, selected, lines } = rigUp(rows(4), { filter: true });
		for (const char of "row-3") view.handleInput(char);
		view.handleInput(ENTER);
		assert.deepStrictEqual(selected, ["row-3"]);
		for (const char of "zzzz") view.handleInput(char);
		assert.ok(lines().some(line => line.includes("no matches")));
	});
});
