/**
 * The transcript's block index: the lazy-lookup maps plus per-turn
 * grouping, isolated from the TUI host so the keying contract is unit-
 * testable headless.
 *
 * The keying law — each map is keyed exactly as narrowly as the protocol
 * scopes its id (the stale-thinking-block bug was a scope collapse: a
 * within-turn id used as a connection-global key, so the next turn's
 * reasoning appended into the previous turn's committed block):
 * - assistant text: one block per **turn** (`turn_id` is connection-wide);
 * - reasoning: per **(turn, id)** — FRONTEND.md §5 scopes the id's
 *   correlation within the turn, and ids repeat across turns;
 * - tools: per `internal_call_id`, connection-unique by contract.
 *
 * Freeze is structural, not tracked: once a turn commits, no later key
 * can address its blocks (a new turn derives new keys), so committed
 * blocks are inert without a freeze flag.
 */

import type { Component } from "@earendil-works/pi-tui";

import type { AssistantBlock } from "./assistant-block.ts";
import type { ReasoningBlock } from "./reasoning-block.ts";
import type { ToolBlock } from "./tool-block.ts";

/** Every block a turn created, for whole-group removal (`turn_retried`). */
export interface TurnEntry {
	component: Component;
	kind: "assistant" | "reasoning" | "tool";
	key: string;
}

export class TranscriptRegistry {
	readonly #assistants = new Map<string, AssistantBlock>();
	readonly #reasonings = new Map<string, ReasoningBlock>();
	readonly #tools = new Map<string, ToolBlock>();
	readonly #turnBlocks = new Map<string, TurnEntry[]>();

	assistant(turnId: string): AssistantBlock | undefined {
		return this.#assistants.get(turnId);
	}

	putAssistant(turnId: string, block: AssistantBlock): void {
		this.#assistants.set(turnId, block);
		this.#track(turnId, { component: block.asComponent(), kind: "assistant", key: turnId });
	}

	reasoning(turnId: string, reasoningId: string): ReasoningBlock | undefined {
		return this.#reasonings.get(reasoningKey(turnId, reasoningId));
	}

	putReasoning(turnId: string, reasoningId: string, block: ReasoningBlock): void {
		const key = reasoningKey(turnId, reasoningId);
		this.#reasonings.set(key, block);
		this.#track(turnId, { component: block.asComponent(), kind: "reasoning", key });
	}

	tool(internalCallId: string): ToolBlock | undefined {
		return this.#tools.get(internalCallId);
	}

	putTool(turnId: string, internalCallId: string, block: ToolBlock): void {
		this.#tools.set(internalCallId, block);
		this.#track(turnId, { component: block.asComponent(), kind: "tool", key: internalCallId });
	}

	/** Thinking lines and tool cards — the collapsible population. */
	collapsibles(): Array<ReasoningBlock | ToolBlock> {
		return [...this.#reasonings.values(), ...this.#tools.values()];
	}

	/** Drop the turn's blocks and index entries; returns what to unhost. */
	removeTurn(turnId: string): TurnEntry[] {
		const entries = this.#turnBlocks.get(turnId) ?? [];
		for (const entry of entries) {
			if (entry.kind === "assistant") this.#assistants.delete(entry.key);
			if (entry.kind === "reasoning") this.#reasonings.delete(entry.key);
			if (entry.kind === "tool") this.#tools.delete(entry.key);
		}
		this.#turnBlocks.delete(turnId);
		return entries;
	}

	clear(): void {
		this.#assistants.clear();
		this.#reasonings.clear();
		this.#tools.clear();
		this.#turnBlocks.clear();
	}

	#track(turnId: string, entry: TurnEntry): void {
		this.#turnBlocks.set(turnId, [...(this.#turnBlocks.get(turnId) ?? []), entry]);
	}
}

function reasoningKey(turnId: string, reasoningId: string): string {
	return `${turnId}:${reasoningId}`;
}
