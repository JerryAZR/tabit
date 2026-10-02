/**
 * The assistant's turn block: streaming markdown. Created on demand by the
 * turn's first text delta — never pre-allocated — and grows by appending
 * increments to its source (the markdown re-derives from the full text).
 * Inset one cell (the transcript gutter, pi's `outputPad`) with no
 * vertical padding — the blank lead line is the air, and extra vertical
 * padding would open a gap before the tool slabs that follow (pi's own
 * stated reason for paddingY=0).
 */

import { Markdown } from "@earendil-works/pi-tui";

import { TranscriptBlock } from "./transcript-block.ts";
import { markdownTheme } from "../theme.ts";

export class AssistantBlock extends TranscriptBlock {
	readonly #markdown = new Markdown("", 1, 0, markdownTheme);
	#text = "";

	constructor(turnId: string, requestRender: () => void) {
		super(turnId, requestRender);
	}

	append(text: string): void {
		this.#text += text;
		this.#markdown.setText(this.#text);
		this.touch();
	}

	text(): string {
		return this.#text;
	}

	override render(width: number): string[] {
		return ["", ...this.#markdown.render(width)];
	}

	override invalidate(): void {
		this.#markdown.invalidate();
	}
}
