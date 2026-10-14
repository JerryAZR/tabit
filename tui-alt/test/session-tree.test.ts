/**
 * The client-built session tree: arrival-order parenting, head moves on
 * `checked_out`, replay union (no re-parenting, no preview doubling), and
 * the pi-ordered flattening (head's subtree first among siblings).
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { SessionTree } from "../src/session-tree.ts";

describe("SessionTree", () => {
	test("a live chain: each entry parents on the previous, rows flatten in order", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "fix the gate");
		tree.openTurn("t1");
		tree.appendTurnText("t1", "On it. ");
		tree.appendTurnText("t1", "Running the tests.");
		tree.addTool("e2", "i1", "bash", "1 file changed");
		assert.strictEqual(tree.headId, "e2");
		const rows = tree.rows();
		assert.deepStrictEqual(rows.map(row => row.id), ["e1", "t1", "e2"]);
		// A single chain stays flat and connector-free — pi renders
		// `├─` only where a parent actually branches; a chain is a list.
		assert.deepStrictEqual(rows.map(row => row.indent), [0, 0, 0]);
		assert.deepStrictEqual(rows.map(row => row.showConnector), [false, false, false]);
		assert.deepStrictEqual(rows.map(row => row.isLast), [true, true, true]);
		// The whole chain is the active path; e2 is the head.
		assert.deepStrictEqual(rows.map(row => row.onActivePath), [true, true, true]);
		assert.strictEqual(rows.find(row => row.id === "e1")!.preview, "fix the gate");
		assert.strictEqual(rows.find(row => row.id === "t1")!.preview, "On it. Running the tests.");
	});

	test("tool rows show the call, learned from the paired tool_call event", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "run tests");
		tree.openTurn("t1");
		tree.noteToolCall("i1", "bash", JSON.stringify({ cmd: "cargo test" }));
		tree.addTool("e2", "i1", "bash", "ok");
		// An unknown call id degrades to the bare name.
		tree.addTool("e3", "i2", "read", "contents");
		const rows = tree.rows();
		assert.strictEqual(rows.find(row => row.id === "e2")!.preview, `bash cmd: cargo test`);
		assert.strictEqual(rows.find(row => row.id === "e3")!.preview, "read");
	});

	test("turn previews only grow while the turn is open — replay never doubles them", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "hi");
		tree.openTurn("t1");
		tree.appendTurnText("t1", "first pass");
		// A re-replay (reconnect, or the post-checkout pass) re-announces t1:
		// the pass closes the open turn first, so the walk re-appends nothing.
		tree.closeTurn();
		tree.openTurn("t1");
		tree.appendTurnText("t1", " — and this delta never happened");
		assert.strictEqual(tree.rows().find(row => row.id === "t1")!.preview, "first pass");
	});

	test("turn_retried drops the turn and its tool results; the head falls back", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "hi");
		tree.openTurn("t1");
		tree.appendTurnText("t1", "draft");
		tree.noteToolCall("i1", "bash", null);
		tree.addTool("e2", "i1", "bash", "…");
		tree.retryTurn("t1");
		assert.strictEqual(tree.size, 1);
		assert.deepStrictEqual(tree.rows().map(row => row.id), ["e1"]);
		assert.strictEqual(tree.headId, "e1");
		// The retry appends to the surviving parent, not to a dropped node.
		tree.openTurn("t2");
		assert.deepStrictEqual(tree.rows().map(row => row.id), ["e1", "t2"]);
	});

	test("checkout moves the head; the next entry branches off it", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "one");
		tree.openTurn("t1");
		tree.appendTurnText("t1", "answer one");
		tree.addUser("e2", "two");
		tree.openTurn("t2");
		tree.appendTurnText("t2", "answer two");
		// Rewind to e1 and continue: the old tail stays, a new branch grows.
		tree.checkout("e1");
		assert.strictEqual(tree.headId, "e1");
		tree.addUser("e3", "fresh start");
		const rows = tree.rows();
		assert.deepStrictEqual(rows.map(row => row.id), ["e1", "e3", "t1", "e2", "t2"]);
		// The head's subtree sorts first among e1's children.
		assert.deepStrictEqual(rows.map(row => row.onActivePath), [true, true, false, false, false]);
		assert.strictEqual(rows.find(row => row.id === "t2")!.isHead, false);
		// The branch point indents, and so does the first generation after it.
		assert.deepStrictEqual(rows.map(row => row.indent), [0, 1, 1, 2, 2]);
	});

	test("post-checkout replay walks the shared prefix: heads resync, nothing re-parents", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "one");
		tree.openTurn("t1");
		tree.appendTurnText("t1", "answer one");
		tree.addUser("e2", "two");
		tree.checkout("e1");
		// The replay announces the rewound chain — ids the store already holds.
		tree.addUser("e1", "one");
		tree.openTurn("t1");
		tree.appendTurnText("t1", "answer one");
		assert.strictEqual(tree.headId, "t1");
		assert.strictEqual(tree.size, 3);
		const rows = tree.rows();
		// e2 is still in the store (the branch survives), just off-path.
		assert.deepStrictEqual(rows.map(row => row.id), ["e1", "t1", "e2"]);
		assert.deepStrictEqual(rows.map(row => row.onActivePath), [true, true, false]);
	});

	test("compaction rows carry the token size", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "hi");
		tree.addCompaction("c1", 291000);
		assert.strictEqual(tree.rows().find(row => row.id === "c1")!.preview, "[compaction: 291k tokens]");
	});

	test("previews are single-line by contract — turn text and tool args collapse", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "rewrite it");
		tree.openTurn("t1");
		// Live turn text carries hard newlines (paragraphs, code blocks).
		tree.appendTurnText("t1", "Strategy (implemented from scratch;\nhigh-level techniques\ndef r(x) {\n  return x;\n}");
		// A write call's `content` argument is multi-line JSON.
		tree.noteToolCall("i1", "write", JSON.stringify({ path: "fastkernel.py", content: '"""\ndoc\n"""\ndef r(...' }));
		tree.addTool("e2", "i1", "write", "ok");
		const rows = tree.rows();
		for (const row of rows) {
			assert.strictEqual(row.preview.includes("\n"), false);
			assert.strictEqual(row.preview.includes("\t"), false);
		}
		assert.ok(rows.find(row => row.id === "t1")!.preview.includes("Strategy (implemented from scratch; high-level"));
		assert.strictEqual(rows.find(row => row.id === "e2")!.preview, `write path: fastkernel.py, content: """ doc """ def r(...`);
	});

	test("reset empties everything (a new session)", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "hi");
		tree.reset();
		assert.strictEqual(tree.size, 0);
		assert.deepStrictEqual(tree.rows(), []);
		assert.strictEqual(tree.headId, null);
	});

	test("rows window past two levels: `│` continues under `├─`, stops under `└─`", () => {
		const tree = new SessionTree();
		tree.addUser("e1", "root");
		tree.openTurn("t1");
		tree.addUser("e2", "child");
		tree.openTurn("t2");
		// Rewind to the root, then branch: the head's new chain sorts first.
		tree.checkout("e1");
		tree.addUser("e3", "sibling");
		tree.openTurn("t3");
		const rows = tree.rows();
		// Head's subtree (e3) first, the abandoned branch after.
		assert.deepStrictEqual(rows.map(row => row.id), ["e1", "e3", "t3", "t1", "e2", "t2"]);
		// Both of e1's children sit at a real branch point — connectors on.
		const e3 = rows.find(row => row.id === "e3")!;
		const t1 = rows.find(row => row.id === "t1")!;
		assert.strictEqual(e3.showConnector, true);
		assert.strictEqual(e3.isLast, false); // the live branch continues with `├─`
		assert.strictEqual(t1.showConnector, true);
		assert.strictEqual(t1.isLast, true); // the abandoned branch closes with `└─`
		// e3's descendants: `│` in e3's column (its `├─` is open).
		const t3 = rows.find(row => row.id === "t3")!;
		assert.deepStrictEqual(t3.gutters, [{ position: 0, show: true }]);
		// t1's descendants: blank in t1's column (its `└─` ends the line).
		const e2 = rows.find(row => row.id === "e2")!;
		assert.deepStrictEqual(e2.gutters, [{ position: 0, show: false }]);
	});
});
