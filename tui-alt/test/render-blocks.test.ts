/**
 * The stateful transcript blocks, rendered headless: a block owns its
 * state and its repaint call, so its render output is assertable without
 * a terminal (the kitty-vt screen harness lands in M1 for the full view).
 */

import assert from "node:assert/strict";
import { setTimeout as sleep } from "node:timers/promises";
import { describe, test } from "node:test";

import { ReasoningBlock } from "../src/components/reasoning-block.ts";
import { StatusBar } from "../src/components/status-bar.ts";
import { ToolBlock } from "../src/components/tool-block.ts";
import type { TuiAltScreen } from "@earendil-works/pi-tui";

const touch = (): number => ++touch.count;
touch.count = 0;

describe("StatusBar", () => {
	test("idle keeps its row as a blank line; busy shows the spinner text", () => {
		const stubTui = { requestRender: () => {} } as unknown as TuiAltScreen;
		const bar = new StatusBar(stubTui, () => {});
		try {
			bar.set("working — esc interrupts");
			assert.ok(bar.render(80).join("").includes("working"));

			bar.set("idle");
			const idle = bar.render(80);
			assert.strictEqual(idle.length, 1); // the reserved row
			assert.strictEqual(idle[0]!.trim(), ""); // blank, not an indicator
		} finally {
			bar.dispose(); // stops the loader's timer
		}
	});
});

describe("ReasoningBlock", () => {
	test("collapsed by default: a counter line, not the text", () => {
		const block = new ReasoningBlock("t1", touch);
		block.append("secret reasoning ");
		block.append("text");
		const lines = block.render(80).join("\n");
		assert.ok(lines.includes("thinking (21 chars)"));
		assert.ok(!lines.includes("secret"));
	});

	test("expanded: the full text, wrapped; toggle and setExpanded flip state", () => {
		const block = new ReasoningBlock("t1", touch);
		block.append("reasoning that is long enough to matter");
		block.setExpanded(true);
		const expanded = block.render(80).join("\n");
		assert.ok(expanded.includes("reasoning that is long enough"));
		block.toggle();
		assert.strictEqual(block.isExpanded(), false);
		assert.ok(block.render(80).join("\n").includes("thinking (39 chars)"));
	});

	test("increments accumulate per block instance", () => {
		const block = new ReasoningBlock("t1", touch);
		block.append("one ");
		block.append("two");
		assert.strictEqual(block.text(), "one two");
	});
});

describe("ToolBlock", () => {
	test("the slab is state-tinted and leads with a blank line", () => {
		const block = new ToolBlock("t1", touch, "grep", "{}"); // unregistered → default handlers
		const lines = block.render(80);
		assert.strictEqual(lines[0], ""); // the breathing-room line
		assert.ok(block.render(80).join("\n").includes("\x1b[48;2;40;40;50m")); // pending slab

		block.setResult("one\ntwo\nthree", true);
		assert.ok(block.render(80).join("\n").includes("\x1b[48;2;40;50;40m")); // success slab

		block.setResult("boom", false);
		assert.ok(block.render(80).join("\n").includes("\x1b[48;2;60;40;40m")); // error slab
	});

	test("adjacent cards keep a blank line apart on the hosted path (asComponent)", () => {
		// The app mounts asComponent(), not render() — the gap must survive
		// that path or consecutive slabs merge (the merged-cards bug).
		const first = new ToolBlock("t1", touch, "grep", "{}");
		first.setResult("a", true);
		const second = new ToolBlock("t1", touch, "grep", "{}");
		second.setResult("b", true);
		const firstSlab = first.asComponent().render(80);
		const secondSlab = second.asComponent().render(80);
		assert.strictEqual(secondSlab[0], "");
		assert.notStrictEqual(firstSlab[firstSlab.length - 1], secondSlab[0]); // never back-to-back slabs
	});

	test("default card: bold-name call line; collapsed previews, expanded shows all", () => {
		const block = new ToolBlock("t1", touch, "grep", "{}");
		const content = Array.from({ length: 9 }, (_, i) => `line-${i + 1}`).join("\n");
		block.setResult(content, true);
		const collapsed = block.render(80).join("\n");
		assert.ok(collapsed.includes("grep")); // the call line
		assert.ok(collapsed.includes("line-5")); // head preview
		assert.ok(!collapsed.includes("line-6"));
		assert.ok(collapsed.includes("more lines, ctrl+o to expand"));

		block.setExpanded(true);
		const expanded = block.render(80).join("\n");
		assert.ok(expanded.includes("line-9")); // no cap expanded
	});

	test("read: the call line is the collapsed view; content only when expanded or failed", () => {
		const args = JSON.stringify({ path: "src/foo.ts", offset: 10, limit: 20 });
		const collapsed = new ToolBlock("t1", touch, "read", args);
		collapsed.setResult("file read", true);
		const collapsedLines = collapsed.render(80).join("\n");
		assert.ok(collapsedLines.includes("read"));
		assert.ok(collapsedLines.includes("src/foo.ts"));
		assert.ok(collapsedLines.includes(":10-29")); // the page range, warn-colored
		assert.ok(!collapsedLines.includes("file read")); // no body collapsed

		const expanded = new ToolBlock("t1", touch, "read", args);
		expanded.setResult("l1\nl2", true);
		expanded.setExpanded(true);
		const expandedLines = expanded.render(80).join("\n");
		assert.ok(expandedLines.indexOf("src/foo.ts") < expandedLines.indexOf("l1"));
		assert.ok(expandedLines.includes("l2"));

		const failed = new ToolBlock("t1", touch, "read", args);
		failed.setResult("no such file", false);
		assert.ok(failed.render(80).join("\n").includes("no such file")); // errors surface collapsed
	});

	test("write: path call line, content preview from the args", () => {
		const content = Array.from({ length: 14 }, (_, i) => `w-${i + 1}`).join("\n");
		const args = JSON.stringify({ path: "out.txt", content });
		const collapsed = new ToolBlock("t1", touch, "write", args);
		collapsed.setResult("written", true);
		const collapsedLines = collapsed.render(80).join("\n");
		assert.ok(collapsedLines.includes("write"));
		assert.ok(collapsedLines.includes("out.txt"));
		assert.ok(collapsedLines.includes("w-10")); // ten-line preview
		assert.ok(!collapsedLines.includes("w-11"));
		assert.ok(collapsedLines.includes("4 more lines"));

		const expanded = new ToolBlock("t1", touch, "write", args);
		expanded.setResult("written", true);
		expanded.setExpanded(true);
		assert.ok(expanded.render(80).join("\n").includes("w-14")); // full content expanded
	});

	test("bash: command call line with timeout suffix; tail preview collapsed, Took line", async () => {
		const plain = new ToolBlock("t1", touch, "bash", JSON.stringify({ command: "npm test" }));
		await sleep(3); // a real call→result gap; the wire carries no durations
		const output = Array.from({ length: 9 }, (_, i) => `o-${i + 1}`).join("\n");
		plain.setResult(output, true);
		const collapsed = plain.render(80).join("\n");
		assert.ok(collapsed.includes("bash npm test"));
		assert.ok(!collapsed.includes("timeout")); // no timeout arg → no suffix
		assert.ok(collapsed.includes("(4 earlier lines, ctrl+o to expand)")); // tail preview
		assert.ok(collapsed.includes("o-9")); // the LAST lines show
		assert.ok(!collapsed.includes("o-4"));
		assert.ok(collapsed.includes("Took 0.0s"));

		const capped = new ToolBlock("t1", touch, "bash", JSON.stringify({ command: "cargo build", timeout_secs: 30 }));
		await sleep(3);
		capped.setResult("done", true);
		const cappedLines = capped.render(80).join("\n");
		assert.ok(cappedLines.includes("bash cargo build"));
		assert.ok(cappedLines.includes("(timeout 30s)"));

		const replay = new ToolBlock("t1", touch, "bash", JSON.stringify({ command: "ls" }));
		replay.setResult("file.txt", true); // same tick → no fabricated duration
		assert.ok(!replay.render(80).join("\n").includes("Took"));
	});

	test("bash expanded shows everything without the hint", async () => {
		const block = new ToolBlock("t1", touch, "bash", JSON.stringify({ command: "ls -la" }));
		await sleep(3);
		block.setResult(Array.from({ length: 12 }, (_, i) => `line-${i + 1}`).join("\n"), true);
		block.setExpanded(true);
		const lines = block.render(80).join("\n");
		assert.ok(lines.indexOf("ls -la") < lines.indexOf("line-1")); // call leads
		assert.ok(lines.includes("line-12")); // full output
		assert.ok(!lines.includes("earlier lines"));
	});

	test("edit: collapsed change summary, expanded unified diff", () => {
		const block = new ToolBlock("t1", touch, "edit", JSON.stringify({ path: "src/foo.ts" }));
		block.setResult("ignored", true, {
			diff: {
				hunks: [
					{
						new_start: 14,
						lines: [
							{ kind: "context", text: "keep" },
							{ kind: "removed", text: "old line" },
							{ kind: "added", text: "new line" },
							{ kind: "added", text: "new line 2" },
						],
					},
				],
			},
			outcomes: [{ index: 0, applied: true }],
		});
		const collapsed = block.render(80).join("\n");
		assert.ok(collapsed.includes("edit"));
		assert.ok(collapsed.includes("src/foo.ts"));
		assert.ok(collapsed.includes("+2 added, -1 deleted"));
		assert.ok(!collapsed.includes("new line"));

		block.setExpanded(true);
		const expanded = block.render(80).join("\n");
		assert.ok(expanded.includes("@@ +14 @@"));
		assert.ok(expanded.includes("+ new line"));
		assert.ok(expanded.includes("- old line"));
		assert.ok(expanded.includes("keep"));
		assert.ok(!expanded.includes("not applied"));
	});

	test("edit marks unapplied hunks", () => {
		const block = new ToolBlock("t1", touch, "edit", JSON.stringify({ path: "src/foo.ts" }));
		block.setResult("ignored", true, {
			diff: { hunks: [{ new_start: 3, lines: [{ kind: "added", text: "x" }] }] },
			outcomes: [{ index: 0, applied: false }],
		});
		block.setExpanded(true);
		assert.ok(block.render(80).join("\n").includes("not applied"));
	});

	test("edit without usable details falls back to the default result; call falls back to the name", () => {
		const block = new ToolBlock("t1", touch, "edit", "{}");
		block.setResult("plain\ncontent", true);
		const collapsed = block.render(80).join("\n");
		assert.ok(collapsed.includes("plain")); // default result previews content
		assert.ok(collapsed.includes("edit")); // default call: the tool name
		block.setExpanded(true);
		const expanded = block.render(80).join("\n");
		assert.ok(expanded.includes("plain"));
		assert.ok(expanded.includes("content"));
	});
});
