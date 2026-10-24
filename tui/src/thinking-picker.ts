/**
 * The `/thinking` picker: the focused model's thinking dial as a
 * navigable list in the dock's card slot — `default` (the null
 * selection, always legal per the wire: the provider/model default)
 * then the announced level names in wire order, the current level
 * marked `✓` and pinned first (the model picker's law). No search line:
 * a dial is 2–5 entries, not a catalog. Enter sends the selection (the
 * `model` command with `thinking_level` — a state write at receive,
 * `model_selected` answers); esc closes.
 *
 * The rows builder is pure and pi-tui-free for tests.
 */

import { getKeybindings, type Component } from "@earendil-works/pi-tui";

import { dim } from "./theme.ts";

export interface ThinkingPickerRow {
	/** The wire value — null is the default row. */
	level: string | null;
	label: string;
	current: boolean;
}

/** Build the picker's rows: `default` first, then the announced names in
 *  wire order; the current level marked and pinned first. `current` is
 *  the stream's fold of `model_selected.thinking_level` — undefined
 *  (not yet announced) marks nothing, null marks the default row. */
export function buildThinkingRows(levels: readonly string[], current: string | null | undefined): ThinkingPickerRow[] {
	const rows: ThinkingPickerRow[] = [
		{ level: null, label: "default", current: current === null },
		...levels.map(name => ({ level: name as string | null, label: name, current: name === current })),
	];
	const currentIndex = rows.findIndex(row => row.current);
	if (currentIndex > 0) {
		const [row] = rows.splice(currentIndex, 1);
		rows.unshift(row!);
	}
	return rows;
}

export interface ThinkingPickerHooks {
	onSelect(level: string | null): void;
	onClose(): void;
}

export class ThinkingPickerView implements Component {
	readonly focusable = true;
	readonly #rows: ThinkingPickerRow[];
	#cursor = 0;
	readonly #hooks: ThinkingPickerHooks;
	readonly #requestRender: () => void;

	constructor(levels: readonly string[], current: string | null | undefined, hooks: ThinkingPickerHooks, requestRender: () => void) {
		this.#rows = buildThinkingRows(levels, current);
		this.#cursor = Math.max(0, this.#rows.findIndex(row => row.current));
		this.#hooks = hooks;
		this.#requestRender = requestRender;
	}

	handleInput(data: string): void {
		const kb = getKeybindings();
		if (kb.matches(data, "tui.select.up")) {
			this.#cursor = this.#cursor === 0 ? this.#rows.length - 1 : this.#cursor - 1;
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.select.down")) {
			this.#cursor = this.#cursor === this.#rows.length - 1 ? 0 : this.#cursor + 1;
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.select.cancel")) {
			this.#hooks.onClose();
			return;
		}
		if (kb.matches(data, "tui.select.confirm")) {
			this.#hooks.onSelect(this.#rows[this.#cursor]!.level);
			return;
		}
	}

	render(width: number): string[] {
		const rule = dim("─".repeat(Math.max(1, width)));
		const lines: string[] = [rule, dim(" thinking level")];
		for (const [index, row] of this.#rows.entries()) {
			const cursor = index === this.#cursor ? "›" : " ";
			const mark = row.current ? " ✓" : "";
			lines.push(` ${cursor} ${row.label}${mark}`);
		}
		lines.push(dim(" enter selects · esc closes"), rule);
		return lines;
	}

	invalidate(): void {}
}
