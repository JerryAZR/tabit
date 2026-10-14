/**
 * One thinking block: accumulates increments per its wire `id` (same id =
 * same block, FRONTEND.md §5 — a provider may spread one continuous
 * thinking segment across tool calls). Collapsed to a counter line by
 * default; expanded in place to the full text — italic gray throughout
 * (pi's treatment: thinking is metadata, not content), inset one cell
 * (the transcript gutter, `outputPad`). Click toggles (the block
 * self-wraps in a MouseRegion); Ctrl+O toggles all via `setExpanded`.
 */

import { MouseRegion, wrapTextWithAnsi, type Component } from "@earendil-works/pi-tui";

import { TranscriptBlock } from "./transcript-block.ts";
import { marker } from "../symbols.ts";
import { thinkingText } from "../theme.ts";

export class ReasoningBlock extends TranscriptBlock {
	readonly #region: MouseRegion;
	#text = "";
	#expanded = false;

	constructor(turnId: string, requestRender: () => void) {
		super(turnId, requestRender);
		const self = this;
		this.#region = new MouseRegion(this, event => {
			if (event.type === "click") {
				self.toggle();
			}
			return undefined;
		});
	}

	append(delta: string): void {
		this.#text += delta;
		this.touch();
	}

	toggle(): void {
		this.#expanded = !this.#expanded;
		this.touch();
	}

	isExpanded(): boolean {
		return this.#expanded;
	}

	setExpanded(expanded: boolean): void {
		this.#expanded = expanded;
		this.touch();
	}

	text(): string {
		return this.#text;
	}

	override render(width: number): string[] {
		if (!this.#expanded) {
			return ["", marker("·", thinkingText(`thinking (${this.#text.length} chars)`))];
		}
		const inner = Math.max(8, width - 3);
		const lines = ["", marker("·", thinkingText("thinking:"))];
		for (const line of wrapTextWithAnsi(this.#text, inner)) lines.push(thinkingText(` ${line}`));
		return lines;
	}

	override invalidate(): void {}

	override asComponent(): Component {
		return this.#region;
	}
}
