/**
 * The two notice surfaces must never be mistakable for each other or for
 * conversation: the dock's notice line is a dim half-frame open onto the
 * editor's top rule; the transcript's NoteBlock is a neutral slate slab
 * with severity-colored content. These tests pin the shapes (structure
 * after stripping ANSI) and the separation of concerns (frame stays dim,
 * slab carries the background).
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { stripTerminalSequences } from "@earendil-works/pi-tui";

import { NoteBlock } from "../src/components/note-block.ts";
import { CompactionBlock } from "../src/components/compaction-block.ts";
import { NoticeBar } from "../src/components/notice-bar.ts";

const noop = () => {};

describe("the docked notice's half-frame", () => {
	test("no live notice renders zero rows", () => {
		const bar = new NoticeBar(noop);
		assert.deepEqual(bar.render(40), []);
		bar.set({ level: "info", text: "something" }, 0);
		bar.set(undefined, 0);
		assert.deepEqual(bar.render(40), []);
	});

	test("a notice renders a top rule and one side-bordered line — no bottom rule", () => {
		const bar = new NoticeBar(noop);
		bar.set({ level: "warn", text: "persistence degraded" }, 0);
		const rows = bar.render(40).map(stripTerminalSequences);
		assert.equal(rows.length, 2);
		assert.equal(rows[0], `┌${"─".repeat(38)}┐`);
		assert.equal(rows[1], "│ ⚠  persistence degraded".padEnd(39) + "│");
	});

	test("the overflow count rides inside the frame", () => {
		const bar = new NoticeBar(noop);
		bar.set({ level: "error", text: "auth failed" }, 2);
		const rows = bar.render(50).map(stripTerminalSequences);
		assert.ok(rows[1]!.includes("✗  auth failed"));
		assert.ok(rows[1]!.includes("+2"));
		assert.ok(rows[1]!.startsWith("│ "));
		assert.ok(rows[1]!.endsWith(" │"));
	});

	test("a long notice wraps inside the side borders, every line full width", () => {
		const bar = new NoticeBar(noop);
		bar.set({ level: "info", text: "a fairly long operational notice that will not fit on one narrow line" }, 0);
		const rows = bar.render(24).map(stripTerminalSequences);
		assert.ok(rows.length > 3);
		for (const row of rows.slice(1)) {
			assert.equal(row.length, 24);
			assert.ok(row.startsWith("│ "));
			assert.ok(row.endsWith(" │"));
		}
	});

	test("the frame is chrome-dim in every severity; severity colors the content only", () => {
		const bar = new NoticeBar(noop);
		bar.set({ level: "error", text: "boom" }, 0);
		const rows = bar.render(30);
		assert.ok(rows[0]!.startsWith("\x1b[2m"));
		assert.ok(!rows[0]!.includes("\x1b[38;2;244;112;103m"));
		assert.ok(rows[1]!.includes("\x1b[38;2;244;112;103m"));
	});
});

describe("the compaction row", () => {
	test("a dedicated purple line carrying before → after, never the info slab", () => {
		const rows = new CompactionBlock(46200, 12000).render(80);
		assert.equal(rows.length, 2);
		assert.equal(rows[0], "");
		assert.equal(stripTerminalSequences(rows[1]!).trimEnd(), "↧  context compacted — 46k → 12k tokens");
		assert.ok(rows[1]!.includes("\x1b[38;2;188;140;240m"));
		// Not the note slab, not SGR faint — the two looks it must never take.
		assert.ok(!rows[1]!.includes("\x1b[48;2;"));
		assert.ok(!rows[1]!.includes("\x1b[2m"));
	});

	test("no metered baseline: the row reports the after length alone", () => {
		const rows = new CompactionBlock(undefined, 4200).render(80);
		assert.ok(stripTerminalSequences(rows[1]!).includes("context now 4.2k tokens"));
	});
});

describe("the transcript note's slab", () => {
	test("a one-line note is the blank lead plus a padded three-row slab", () => {
		const note = new NoteBlock("run failed (model): no key", "error");
		const rows = note.render(60);
		assert.equal(rows.length, 4);
		assert.equal(rows[0], "");
		for (const row of rows.slice(1)) assert.ok(row.includes("\x1b[48;2;36;36;42m"));
		assert.equal(stripTerminalSequences(rows[1]!).trim(), "");
		assert.ok(stripTerminalSequences(rows[2]!).includes("✗  run failed (model): no key"));
		assert.equal(stripTerminalSequences(rows[3]!).trim(), "");
	});

	test("severity colors the content: an error note is coral, an info note dim", () => {
		const errorRows = new NoteBlock("boom", "error").render(40);
		const infoRows = new NoteBlock("boom", "info").render(40);
		assert.ok(errorRows[2]!.includes("\x1b[38;2;244;112;103m"));
		assert.ok(infoRows[2]!.includes("\x1b[2m"));
	});

	test("the slab is not the user bubble's gray", () => {
		const rows = new NoteBlock("rewound", "info").render(40);
		assert.ok(!rows.join("\n").includes("\x1b[48;2;52;53;65m"));
	});
});
