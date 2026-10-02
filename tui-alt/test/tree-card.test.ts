/**
 * The session-tree card, headless: pi's tree-selector key model over the
 * client-built store (↑/↓ wrap, ←/→ page, enter rewinds or closes on the
 * head, escape closes) and the painted row language (connectors, `•` path
 * marks, cursor, status line).
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { TreeCardView } from "../src/components/tree-card.ts";
import { applyKeybindings } from "../src/keybindings.ts";
import { SessionTree } from "../src/session-tree.ts";

// Rows resolve the select actions through the global registry — installed
// in production by root.bind. Install the defaults here.
applyKeybindings();

const UP = "\x1b[A";
const DOWN = "\x1b[B";
const LEFT = "\x1b[D";
const RIGHT = "\x1b[C";
const ENTER = "\r";
const ESCAPE = "\x1b";

interface Event {
	kind: "checkout" | "close";
	entryId?: string;
}

function driven(build: (tree: SessionTree) => void, keys: string[]): { events: Event[]; lines: string[] } {
	const tree = new SessionTree();
	build(tree);
	const events: Event[] = [];
	const view = new TreeCardView(
		tree,
		{
			onCheckout: entryId => events.push({ kind: "checkout", entryId }),
			onClose: () => events.push({ kind: "close" }),
		},
		() => {},
	);
	for (const key of keys) view.handleInput(key);
	return { events, lines: view.render(80) };
}

describe("TreeCardView", () => {
	const grow = (tree: SessionTree): void => {
		tree.addUser("e1", "first question");
		tree.openTurn("t1");
		tree.appendTurnText("t1", "first answer");
		tree.addUser("e2", "second question");
		tree.openTurn("t2");
		tree.appendTurnText("t2", "second answer");
	};

	test("enter on a rewound row sends checkout; escape closes; enter on the head just closes", () => {
		const rewound = driven(
			tree => {
				grow(tree);
				tree.checkout("e1"); // head is now e1, not the last row
			},
			[UP, ENTER], // move off the head (to t2), rewind there
		);
		assert.deepStrictEqual(rewound.events, [{ kind: "checkout", entryId: "t2" }]);

		const closed = driven(tree => {
			grow(tree);
			tree.checkout("e1");
		}, [ESCAPE]);
		assert.deepStrictEqual(closed.events, [{ kind: "close" }]);

		const alreadyHere = driven(grow, [DOWN, DOWN, DOWN, ENTER]); // cursor to t2, the head
		assert.deepStrictEqual(alreadyHere.events, [{ kind: "close" }]);
	});

	test("cursor wraps both ways; arrows page", () => {
		const wrapped = driven(tree => {
			grow(tree);
			tree.checkout("e1");
		}, [UP, ENTER]); // from the head (e1, index 0) wrap to t2 (last row)
		assert.deepStrictEqual(wrapped.events, [{ kind: "checkout", entryId: "t2" }]);

		const paged = driven(tree => {
			grow(tree);
			tree.checkout("e1");
		}, [LEFT, ENTER]); // page up clamps at 0 — still e1, the head → close
		assert.deepStrictEqual(paged.events, [{ kind: "close" }]);

		const pageDown = driven(grow, [LEFT, RIGHT, ENTER]); // 0 → page → clamp to t2 (the head) → close
		assert.deepStrictEqual(pageDown.events, [{ kind: "close" }]);
	});

	test("rows paint the tree language: cursor, active-path dot, typed content", () => {
		const view = driven(tree => {
			grow(tree);
			tree.checkout("e1");
			tree.addUser("e3", "fresh start"); // branch: e3 on path, old tail off it
		}, []);
		const rows = view.lines.filter(line => line.includes("user:") || line.includes("assistant:"));
		assert.strictEqual(rows.length, 5);
		// The head's subtree first; the active-path rows carry the accent dot.
		assert.ok(rows[0].includes("user: "));
		assert.ok(rows[0].includes("first question"));
		assert.ok(rows[0]!.includes("\x1b[36m• "));
		// Off-path rows carry no dot.
		assert.ok(!rows[2].includes("• "));
		// The cursor leads the first row; the second (a branch child) leads
		// with its unselected gutter and a `├─` connector.
		assert.strictEqual(rows[0]!.startsWith(" ❯ "), true);
		assert.ok(rows[1].includes("├─"));
		// Status line with the count and the hint.
		assert.strictEqual(view.lines.some(line => line.includes("(1/5)") && line.includes("enter rewinds")), true);
	});

	test("an empty session renders a quiet card; enter does nothing", () => {
		const empty = driven(() => {}, [ENTER, ESCAPE]);
		assert.deepStrictEqual(empty.events, [{ kind: "close" }]);
		assert.strictEqual(empty.lines.length, 3);
		assert.ok(empty.lines[0].includes("─"));
		assert.ok(empty.lines[1].includes("empty"));
		assert.ok(empty.lines[2].includes("─"));
	});
});
