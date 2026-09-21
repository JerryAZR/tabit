/**
 * A transcript note: lifecycle and error notices that are not part of the
 * conversation (errors, persistence warnings, discards, rewinds). Static
 * content — implements the engine's Component directly. Leads with the
 * uniform blank line; no trailing spacer (the next block owns its own air).
 */

import { Text, type Component } from "@earendil-works/pi-tui";

import { marker } from "../symbols";

export class NoteBlock implements Component {
	readonly #line: Text;

	constructor(text: string, kind: "info" | "warn" | "error") {
		const mark = kind === "error" ? "✗" : kind === "warn" ? "⚠" : "·";
		this.#line = new Text(marker(mark, text));
	}

	render(width: number): string[] {
		return ["", ...this.#line.render(width)];
	}

	invalidate(): void {
		this.#line.invalidate();
	}
}
