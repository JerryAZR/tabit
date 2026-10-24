/**
 * The session-tree card (M2's rewind surface): pi's tree-selector
 * presentation over the client-built `SessionTree`, hosted in the dock's
 * card slot like an interaction card — while it is open the input listener
 * stands down and it owns the keyboard.
 *
 * Keys: ↑/↓ move (wrapping, pi's tree rule), ←/→ page, enter rewinds —
 * `checkout` composes abort backend-side, and the following replay pass is
 * the re-render, so the card just sends and closes — escape closes. Enter
 * on the chain head is pi's "already at this point": it closes without
 * sending. Rows paint pi's language: a dim prefix of `│` gutters and
 * `├─`/`└─` connectors, an accent `•` on the active path, typed content
 * (`user:`, `assistant:`, the muted tool call, the compaction marker), and
 * a muted `(n/total)` status line.
 */

import { getKeybindings, truncateToWidth, type Component } from "@earendil-works/pi-tui";

import { accent, dim, success, toolMuted, warn } from "../theme.ts";
import type { SessionTree, TreeRow } from "../session-tree.ts";

export interface TreeCardHooks {
	onCheckout(entryId: string): void;
	onClose(): void;
}

/** Bounded viewport — the dock card must not grow with the session. */
const MAX_VISIBLE_ROWS = 15;

export class TreeCardView implements Component {
	readonly focusable = true;
	#cursor = 0;
	readonly #tree: SessionTree;
	readonly #hooks: TreeCardHooks;
	readonly #requestRender: () => void;

	constructor(tree: SessionTree, hooks: TreeCardHooks, requestRender: () => void) {
		this.#tree = tree;
		this.#hooks = hooks;
		this.#requestRender = requestRender;
	}

	handleInput(data: string): void {
		const kb = getKeybindings();
		const total = this.#tree.size;
		if (total === 0) {
			if (kb.matches(data, "tui.select.cancel")) this.#hooks.onClose();
			return;
		}
		if (kb.matches(data, "tui.select.up")) {
			this.#cursor = this.#cursor === 0 ? total - 1 : this.#cursor - 1;
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.select.down")) {
			this.#cursor = this.#cursor === total - 1 ? 0 : this.#cursor + 1;
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.editor.cursorLeft")) {
			this.#cursor = Math.max(0, this.#cursor - MAX_VISIBLE_ROWS);
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.editor.cursorRight")) {
			this.#cursor = Math.min(total - 1, this.#cursor + MAX_VISIBLE_ROWS);
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.select.cancel")) {
			this.#hooks.onClose();
			return;
		}
		if (kb.matches(data, "tui.select.confirm")) {
			const rows = this.#tree.rows();
			const row = rows[this.#cursor];
			if (row === undefined) return;
			// The head is where the session already stands — pi closes with
			// "Already at this point" rather than sending a no-op command.
			if (!row.isHead) this.#hooks.onCheckout(row.id);
			else this.#hooks.onClose();
		}
	}

	invalidate(): void {}

	render(width: number): string[] {
		const rows = this.#tree.rows();
		const rule = dim("─".repeat(Math.max(1, width)));
		if (rows.length === 0) {
			return [rule, dim(" session tree — empty"), rule];
		}
		this.#cursor = Math.min(this.#cursor, rows.length - 1);
		const startIndex = Math.max(
			0,
			Math.min(this.#cursor - Math.floor(MAX_VISIBLE_ROWS / 2), rows.length - MAX_VISIBLE_ROWS),
		);
		const endIndex = Math.min(startIndex + MAX_VISIBLE_ROWS, rows.length);
		const lines: string[] = [rule];
		for (let index = startIndex; index < endIndex; index++) {
			lines.push(truncateToWidth(this.#row(rows[index]!, index === this.#cursor), width));
		}
		const atHead = rows[this.#cursor]!.isHead;
		lines.push(truncateToWidth(dim(` (${this.#cursor + 1}/${rows.length}) · ${atHead ? "already here · esc closes" : "enter rewinds · esc closes"}`), width));
		lines.push(rule);
		return lines;
	}

	#row(row: TreeRow, selected: boolean): string {
		const cursor = selected ? "❯ " : "  ";
		const prefix = this.#prefix(row);
		const path = row.onActivePath ? accent("• ") : "";
		return ` ${cursor}${dim(prefix)}${path}${this.#content(row)}`;
	}

	/** pi's prefix: three cells per level — `│` (or blank, below a closed
	 *  branch) where an ancestor's connector hangs, the row's own
	 *  `├─`/`└─` connector in its last level. */
	#prefix(row: TreeRow): string {
		const chars: string[] = [];
		const total = row.indent * 3;
		const connectorPosition = row.showConnector ? row.indent - 1 : -1;
		for (let index = 0; index < total; index += 1) {
			const level = Math.floor(index / 3);
			const gutter = row.gutters.find(candidate => candidate.position === level);
			if (gutter !== undefined) {
				chars.push(index % 3 === 0 ? (gutter.show ? "│" : " ") : " ");
			} else if (level === connectorPosition) {
				if (index % 3 === 0) chars.push(row.isLast ? "└" : "├");
				else if (index % 3 === 1) chars.push("─");
				else chars.push(" ");
			} else {
				chars.push(" ");
			}
		}
		return chars.join("");
	}

	#content(row: TreeRow): string {
		switch (row.kind) {
			case "user":
				return accent("user: ") + row.preview;
			case "turn":
				return success("assistant: ") + (row.preview === "" ? dim("(no text)") : row.preview);
			case "tool":
				return toolMuted(row.preview);
			case "compaction":
				return warn(row.preview);
		}
	}
}
