/**
 * The `/thinking` picker's spec for the shared SelectorView: the focused
 * model's thinking dial as a navigable list — `default` (the null
 * selection, always legal per the wire: the provider/model default)
 * then the announced level names in wire order, the current level
 * marked `✓` and pinned first (the model picker's law). No search line
 * and no viewport: a dial is 2–5 entries, not a catalog — the one-column
 * degenerate case of the selector's row renderer. Enter sends the
 * selection (the `model` command with `thinking_level`); esc closes.
 *
 * The rows builder is pure and pi-tui-free for tests.
 */

import type { SelectorSpec } from "./components/selector.ts";

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

/** The thinking dial's SelectorView spec: a title, the bare-label rows,
 *  no filter, no pageSize. */
export function thinkingPickerSpec(levels: readonly string[], current: string | null | undefined): SelectorSpec<ThinkingPickerRow, void> {
	return {
		entries: buildThinkingRows(levels, current),
		title: " thinking level",
		row: row => `${row.label}${row.current ? " ✓" : ""}`,
	};
}
