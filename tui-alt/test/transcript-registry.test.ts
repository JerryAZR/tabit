/**
 * The transcript registry, headless: the keying law (maps keyed exactly
 * as narrowly as the protocol scopes their ids) and the per-turn grouping
 * that `turn_retried` removal depends on. The stale-thinking-block bug
 * lives here when it regresses: a within-turn reasoning id used as a
 * connection-global key let the next turn's thinking append into the
 * previous turn's committed block.
 */

import { describe, expect, test } from "bun:test";

import { AssistantBlock } from "../src/components/assistant-block";
import { ReasoningBlock } from "../src/components/reasoning-block";
import { ToolBlock } from "../src/components/tool-block";
import { TranscriptRegistry } from "../src/components/transcript-registry";

const touch = () => {};

describe("TranscriptRegistry", () => {
	test("the same reasoning id in a second turn is a new block, never the committed one", () => {
		const registry = new TranscriptRegistry();
		registry.putReasoning("t1", "r1", new ReasoningBlock("t1", touch));
		const first = registry.reasoning("t1", "r1");
		first!.append("turn one thinking");

		// Turn 2 reuses the id (the mock's own habit): it must not find t1's block.
		let created = 0;
		if (registry.reasoning("t2", "r1") === undefined) created++;
		expect(created).toBe(1);
		registry.putReasoning("t2", "r1", new ReasoningBlock("t2", touch));
		expect(registry.reasoning("t2", "r1")).not.toBe(first);
		expect(registry.reasoning("t1", "r1")).toBe(first); // t1's block untouched
		expect(first!.text()).toBe("turn one thinking"); // frozen: no cross-turn append
	});

	test("removeTurn drops exactly the turn's blocks from every map", () => {
		const registry = new TranscriptRegistry();
		registry.putAssistant("t1", new AssistantBlock("t1", touch));
		registry.putReasoning("t1", "r1", new ReasoningBlock("t1", touch));
		registry.putTool("t1", "call-1", new ToolBlock("t1", touch, "bash", null));
		registry.putReasoning("t2", "r1", new ReasoningBlock("t2", touch)); // same id, other turn

		const removed = registry.removeTurn("t1");
		expect(removed).toHaveLength(3);
		expect(registry.assistant("t1")).toBeUndefined();
		expect(registry.reasoning("t1", "r1")).toBeUndefined();
		expect(registry.tool("call-1")).toBeUndefined();
		expect(registry.reasoning("t2", "r1")).toBeDefined(); // survives
		expect(registry.removeTurn("t1")).toEqual([]); // second remove is a no-op
	});

	test("collapsibles lists reasoning and tool blocks; clear empties everything", () => {
		const registry = new TranscriptRegistry();
		registry.putReasoning("t1", "r1", new ReasoningBlock("t1", touch));
		registry.putTool("t1", "call-1", new ToolBlock("t1", touch, "read", null));
		registry.putAssistant("t1", new AssistantBlock("t1", touch)); // not collapsible
		expect(registry.collapsibles()).toHaveLength(2);

		registry.clear();
		expect(registry.collapsibles()).toHaveLength(0);
		expect(registry.assistant("t1")).toBeUndefined();
		expect(registry.removeTurn("t1")).toEqual([]);
	});
});
