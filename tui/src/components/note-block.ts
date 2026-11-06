/**
 * A transcript note: lifecycle and error notices that are not part of the
 * conversation (errors, persistence warnings, discards, rewinds) — history
 * annotations, replayed at their position in the session timeline. They
 * get their own slab: a neutral slate background with severity-colored
 * marker and text, so a note is unmistakably neither conversation (the
 * user bubble's gray), nor a tool outcome (state-tinted slabs), nor
 * thinking (italic gray). The slab breathes: one tinted row of air above
 * and below (the user bubble's `Box(1,1)` construction) — an unpadded
 * fill reads as a highlighter strip, not a card. Leads with the uniform
 * blank line; no trailing spacer (the next block owns its own air).
 */

import { Box, Text, type Component } from "@earendil-works/pi-tui";

import { marker } from "../symbols.ts";
import { dim, error, noteBg, warn } from "../theme.ts";

export class NoteBlock implements Component {
	readonly #box: Box;

	constructor(text: string, kind: "info" | "warn" | "error") {
		const color = kind === "error" ? error : kind === "warn" ? warn : dim;
		const mark = kind === "error" ? "✗" : kind === "warn" ? "⚠" : "·";
		this.#box = new Box(1, 1, noteBg);
		this.#box.addChild(new Text(color(marker(mark, text)), 0, 0));
	}

	render(width: number): string[] {
		return ["", ...this.#box.render(width)];
	}

	invalidate(): void {
		this.#box.invalidate();
	}
}
