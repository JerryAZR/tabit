/**
 * The dock's notice region (v27): the one home for every backend
 * notice — operational reports, recovered degradations, blocking
 * conditions. Renders the highest-severity live notice with an
 * overflow count; zero live notices render zero rows (the
 * PendingQueue pattern). Severity owns the color; lifetime is the
 * mode's ruling (errors pin until `notice_cleared`, info/warn age on
 * the turn clock) — this component only renders what it is handed.
 * The transcript is conversation; nothing here prints there.
 */

import { Container, Text, type Component } from "@earendil-works/pi-tui";

import { marker } from "../symbols.ts";
import { dim, error, warn } from "../theme.ts";

export type NoticeLevel = "info" | "warn" | "error";

export interface DockedNotice {
	level: NoticeLevel;
	text: string;
}

export class NoticeBar implements Component {
	readonly #container = new Container();
	readonly #requestRender: () => void;

	constructor(requestRender: () => void) {
		this.#requestRender = requestRender;
	}

	/** `notice` is the highest-severity live one (the mode's pick);
	 *  `overflow` counts the rest. Undefined clears the row. */
	set(notice: DockedNotice | undefined, overflow: number): void {
		this.#container.clear();
		if (notice !== undefined) {
			const color = notice.level === "error" ? error : notice.level === "warn" ? warn : dim;
			const glyph = notice.level === "error" ? "✗" : notice.level === "warn" ? "⚠" : "·";
			const more = overflow > 0 ? dim(` · +${overflow}`) : "";
			this.#container.addChild(new Text(marker(glyph, color(notice.text) + more)));
		}
		this.#requestRender();
	}

	render(width: number): string[] {
		return this.#container.render(width);
	}

	invalidate(): void {
		this.#container.invalidate();
	}
}
