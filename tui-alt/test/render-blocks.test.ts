/**
 * The stateful transcript blocks, rendered headless: a block owns its
 * state and its repaint call, so its render output is assertable without
 * a terminal (the kitty-vt screen harness lands in M1 for the full view).
 */

import { describe, expect, test } from "bun:test";

import { ReasoningBlock } from "../src/components/reasoning-block";
import { StatusBar } from "../src/components/status-bar";
import { ToolBlock } from "../src/components/tool-block";
import type { TuiAltScreen } from "@earendil-works/pi-tui";

const touch = (): number => ++touch.count;
touch.count = 0;

describe("StatusBar", () => {
	test("idle keeps its row as a blank line; busy shows the spinner text", () => {
		const stubTui = { requestRender: () => {} } as unknown as TuiAltScreen;
		const bar = new StatusBar(stubTui, () => {});
		try {
			bar.set("working — esc interrupts");
			expect(bar.render(80).join("")).toContain("working");

			bar.set("idle");
			const idle = bar.render(80);
			expect(idle).toHaveLength(1); // the reserved row
			expect(idle[0]!.trim()).toBe(""); // blank, not an indicator
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
		expect(lines).toContain("thinking (21 chars)");
		expect(lines).not.toContain("secret");
	});

	test("expanded: the full text, wrapped; toggle and setExpanded flip state", () => {
		const block = new ReasoningBlock("t1", touch);
		block.append("reasoning that is long enough to matter");
		block.setExpanded(true);
		const expanded = block.render(80).join("\n");
		expect(expanded).toContain("reasoning that is long enough");
		block.toggle();
		expect(block.isExpanded()).toBe(false);
		expect(block.render(80).join("\n")).toContain("thinking (39 chars)");
	});

	test("increments accumulate per block instance", () => {
		const block = new ReasoningBlock("t1", touch);
		block.append("one ");
		block.append("two");
		expect(block.text()).toBe("one two");
	});
});

describe("ToolBlock", () => {
	test("the slab is state-tinted and leads with a blank line", () => {
		const block = new ToolBlock("t1", touch, "grep", "{}"); // unregistered → default handlers
		const lines = block.render(80);
		expect(lines[0]).toBe(""); // the breathing-room line
		expect(block.render(80).join("\n")).toContain("\x1b[48;2;40;40;50m"); // pending slab

		block.setResult("one\ntwo\nthree", true);
		expect(block.render(80).join("\n")).toContain("\x1b[48;2;40;50;40m"); // success slab

		block.setResult("boom", false);
		expect(block.render(80).join("\n")).toContain("\x1b[48;2;60;40;40m"); // error slab
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
		expect(secondSlab[0]).toBe("");
		expect(firstSlab[firstSlab.length - 1]).not.toBe(secondSlab[0]); // never back-to-back slabs
	});

	test("default card: bold-name call line; collapsed previews, expanded shows all", () => {
		const block = new ToolBlock("t1", touch, "grep", "{}");
		const content = Array.from({ length: 9 }, (_, i) => `line-${i + 1}`).join("\n");
		block.setResult(content, true);
		const collapsed = block.render(80).join("\n");
		expect(collapsed).toContain("grep"); // the call line
		expect(collapsed).toContain("line-5"); // head preview
		expect(collapsed).not.toContain("line-6");
		expect(collapsed).toContain("more lines, ctrl+o to expand");

		block.setExpanded(true);
		const expanded = block.render(80).join("\n");
		expect(expanded).toContain("line-9"); // no cap expanded
	});

	test("read: the call line is the collapsed view; content only when expanded or failed", () => {
		const args = JSON.stringify({ path: "src/foo.ts", offset: 10, limit: 20 });
		const collapsed = new ToolBlock("t1", touch, "read", args);
		collapsed.setResult("file read", true);
		const collapsedLines = collapsed.render(80).join("\n");
		expect(collapsedLines).toContain("read");
		expect(collapsedLines).toContain("src/foo.ts");
		expect(collapsedLines).toContain(":10-29"); // the page range, warn-colored
		expect(collapsedLines).not.toContain("file read"); // no body collapsed

		const expanded = new ToolBlock("t1", touch, "read", args);
		expanded.setResult("l1\nl2", true);
		expanded.setExpanded(true);
		const expandedLines = expanded.render(80).join("\n");
		expect(expandedLines.indexOf("src/foo.ts")).toBeLessThan(expandedLines.indexOf("l1"));
		expect(expandedLines).toContain("l2");

		const failed = new ToolBlock("t1", touch, "read", args);
		failed.setResult("no such file", false);
		expect(failed.render(80).join("\n")).toContain("no such file"); // errors surface collapsed
	});

	test("write: path call line, content preview from the args", () => {
		const content = Array.from({ length: 14 }, (_, i) => `w-${i + 1}`).join("\n");
		const args = JSON.stringify({ path: "out.txt", content });
		const collapsed = new ToolBlock("t1", touch, "write", args);
		collapsed.setResult("written", true);
		const collapsedLines = collapsed.render(80).join("\n");
		expect(collapsedLines).toContain("write");
		expect(collapsedLines).toContain("out.txt");
		expect(collapsedLines).toContain("w-10"); // ten-line preview
		expect(collapsedLines).not.toContain("w-11");
		expect(collapsedLines).toContain("4 more lines");

		const expanded = new ToolBlock("t1", touch, "write", args);
		expanded.setResult("written", true);
		expanded.setExpanded(true);
		expect(expanded.render(80).join("\n")).toContain("w-14"); // full content expanded
	});

	test("bash: command call line with timeout suffix; tail preview collapsed, Took line", async () => {
		const plain = new ToolBlock("t1", touch, "bash", JSON.stringify({ command: "npm test" }));
		await Bun.sleep(3); // a real call→result gap; the wire carries no durations
		const output = Array.from({ length: 9 }, (_, i) => `o-${i + 1}`).join("\n");
		plain.setResult(output, true);
		const collapsed = plain.render(80).join("\n");
		expect(collapsed).toContain("bash npm test");
		expect(collapsed).not.toContain("timeout"); // no timeout arg → no suffix
		expect(collapsed).toContain("(4 earlier lines, ctrl+o to expand)"); // tail preview
		expect(collapsed).toContain("o-9"); // the LAST lines show
		expect(collapsed).not.toContain("o-4");
		expect(collapsed).toContain("Took 0.0s");

		const capped = new ToolBlock("t1", touch, "bash", JSON.stringify({ command: "cargo build", timeout_secs: 30 }));
		await Bun.sleep(3);
		capped.setResult("done", true);
		const cappedLines = capped.render(80).join("\n");
		expect(cappedLines).toContain("bash cargo build");
		expect(cappedLines).toContain("(timeout 30s)");

		const replay = new ToolBlock("t1", touch, "bash", JSON.stringify({ command: "ls" }));
		replay.setResult("file.txt", true); // same tick → no fabricated duration
		expect(replay.render(80).join("\n")).not.toContain("Took");
	});

	test("bash expanded shows everything without the hint", async () => {
		const block = new ToolBlock("t1", touch, "bash", JSON.stringify({ command: "ls -la" }));
		await Bun.sleep(3);
		block.setResult(Array.from({ length: 12 }, (_, i) => `line-${i + 1}`).join("\n"), true);
		block.setExpanded(true);
		const lines = block.render(80).join("\n");
		expect(lines.indexOf("ls -la")).toBeLessThan(lines.indexOf("line-1")); // call leads
		expect(lines).toContain("line-12"); // full output
		expect(lines).not.toContain("earlier lines");
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
		expect(collapsed).toContain("edit");
		expect(collapsed).toContain("src/foo.ts");
		expect(collapsed).toContain("+2 added, -1 deleted");
		expect(collapsed).not.toContain("new line");

		block.setExpanded(true);
		const expanded = block.render(80).join("\n");
		expect(expanded).toContain("@@ +14 @@");
		expect(expanded).toContain("+ new line");
		expect(expanded).toContain("- old line");
		expect(expanded).toContain("keep");
		expect(expanded).not.toContain("not applied");
	});

	test("edit marks unapplied hunks", () => {
		const block = new ToolBlock("t1", touch, "edit", JSON.stringify({ path: "src/foo.ts" }));
		block.setResult("ignored", true, {
			diff: { hunks: [{ new_start: 3, lines: [{ kind: "added", text: "x" }] }] },
			outcomes: [{ index: 0, applied: false }],
		});
		block.setExpanded(true);
		expect(block.render(80).join("\n")).toContain("not applied");
	});

	test("edit without usable details falls back to the default result; call falls back to the name", () => {
		const block = new ToolBlock("t1", touch, "edit", "{}");
		block.setResult("plain\ncontent", true);
		const collapsed = block.render(80).join("\n");
		expect(collapsed).toContain("plain"); // default result previews content
		expect(collapsed).toContain("edit"); // default call: the tool name
		block.setExpanded(true);
		const expanded = block.render(80).join("\n");
		expect(expanded).toContain("plain");
		expect(expanded).toContain("content");
	});
});
