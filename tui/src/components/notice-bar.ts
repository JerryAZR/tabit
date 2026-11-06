/**
 * The dock's notice region (v27): the one home for every backend
 * notice — operational reports, recovered degradations, blocking
 * conditions. Renders the highest-severity live notice with an
 * overflow count; zero live notices render zero rows (the
 * PendingQueue pattern). Severity owns the content's color; lifetime is
 * the mode's ruling (errors pin until `notice_cleared`, info/warn age on
 * the turn clock) — this component only renders what it is handed.
 * The transcript is conversation; nothing here prints there.
 *
 * The half-frame is what makes the region read as docked chrome rather
 * than transcript content: a top rule and side verticals in the editor
 * border's dim, open at the bottom, where the editor's own top rule
 * closes the shape. The frame stays dim in every severity — the colored
 * marker and text carry the severity, the frame carries the dockedness.
 */

import { Text, visibleWidth, type Component } from "@earendil-works/pi-tui";

import { marker } from "../symbols.ts";
import { dim, error, warn } from "../theme.ts";

export type NoticeLevel = "info" | "warn" | "error";

export interface DockedNotice {
	level: NoticeLevel;
	text: string;
}

export class NoticeBar implements Component {
	readonly #requestRender: () => void;
	#line: Text | undefined;

	constructor(requestRender: () => void) {
		this.#requestRender = requestRender;
	}

	/** `notice` is the highest-severity live one (the mode's pick);
	 *  `overflow` counts the rest. Undefined clears the row. */
	set(notice: DockedNotice | undefined, overflow: number): void {
		if (notice === undefined) {
			this.#line = undefined;
		} else {
			const color = notice.level === "error" ? error : notice.level === "warn" ? warn : dim;
			const glyph = notice.level === "error" ? "✗" : notice.level === "warn" ? "⚠" : "·";
			const more = overflow > 0 ? dim(` · +${overflow}`) : "";
			// Zero padding: the frame owns the notice's air, not the Text.
			this.#line = new Text(marker(glyph, color(notice.text) + more), 0, 0);
		}
		this.#requestRender();
	}

	render(width: number): string[] {
		if (this.#line === undefined) return [];
		const rows = [dim(`┌${"─".repeat(Math.max(0, width - 2))}┐`)];
		for (const line of this.#line.render(Math.max(1, width - 4))) {
			const pad = " ".repeat(Math.max(0, width - 4 - visibleWidth(line)));
			rows.push(`${dim("│")} ${line}${pad} ${dim("│")}`);
		}
		return rows;
	}

	invalidate(): void {
		this.#line?.invalidate();
	}
}
