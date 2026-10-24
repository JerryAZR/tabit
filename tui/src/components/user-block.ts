/**
 * The user's message — pi's shape: a tinted slab (Box, 1×1 padding, the
 * `#343541` bubble gray) under one blank line, near-white text. The slab
 * is what makes turns scannable; the blank lead is the transcript's
 * uniform rhythm (every block owns exactly one line of air above).
 * Static content — implements the engine's Component directly.
 */

import { Box, Text, type Component } from "@earendil-works/pi-tui";

import { userMessageBg, userMessageText } from "../theme.ts";

export class UserBlock implements Component {
	readonly #box: Box;

	constructor(text: string) {
		this.#box = new Box(1, 1, userMessageBg);
		this.#box.addChild(new Text(userMessageText(text), 0, 0));
	}

	render(width: number): string[] {
		return ["", ...this.#box.render(width)];
	}

	invalidate(): void {
		this.#box.invalidate();
	}
}
