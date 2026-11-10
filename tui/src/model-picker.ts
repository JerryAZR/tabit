/**
 * The `/model` picker's spec for the shared SelectorView: pi's selector
 * pattern (a search line over sparse rows) with the owner-ruled three
 * aligned columns — name | context window | provider — the current
 * register pinned first under a `✓`; a register outside the catalog
 * simply has no `✓` (owner ruling: the footer carries the current
 * state, the picker needn't synthesize one). Picking sends the `model`
 * command with no thinking_level (null is always legal).
 *
 * The rows builder and filter are pure and pi-tui-free for tests; the
 * SelectorView owns the cursor, viewport, and chrome.
 */

import { fuzzyFilter } from "@earendil-works/pi-tui";

import type { SelectorSpec } from "./components/selector.ts";
import { accent, dim, warn } from "./theme.ts";
import type { AvailableProvider } from "./protocol.ts";

/** The session's register, for the current marker. */
export interface ModelPickerCurrent {
	provider: string;
	model: string;
}

/** One selectable row: the command address (provider id, model id) plus
 *  its display columns. */
export interface ModelPickerRow {
	provider: string;
	model: string;
	/** Display name — `name` ?? `id` (the contract's fallback rule). */
	name: string;
	/** Compact context window (`200k`), or "" when the config doesn't
	 *  state one (the v11 rule: absent means unstated, never zero). */
	contextWindow: string;
	/** Provider display — `name` ?? `id`. */
	providerName: string;
	current: boolean;
}

/** A compact token count: 200000 → `200k`, 8192 → `8.2k`, 1_000_000 → `1M`. */
export function compactTokens(tokens: number | undefined): string {
	if (tokens === undefined) return "";
	if (tokens >= 1_000_000) return `${Math.round(tokens / 100_000) / 10}M`;
	if (tokens >= 1000) return `${Math.round(tokens / 100) / 10}k`;
	return String(tokens);
}

/** Build the picker's rows from the catalog: the current register pinned
 *  first, then plain wire order (providers alphabetical, models in
 *  config-file order — display sorting is the frontend's; keep it
 *  boring). */
export function buildModelRows(catalog: readonly AvailableProvider[], current: ModelPickerCurrent | undefined): ModelPickerRow[] {
	const rows: ModelPickerRow[] = [];
	for (const provider of catalog) {
		for (const model of provider.models) {
			rows.push({
				provider: provider.id,
				model: model.id,
				name: model.name ?? model.id,
				contextWindow: compactTokens(model.context_window),
				providerName: provider.name ?? provider.id,
				current: current !== undefined && current.provider === provider.id && current.model === model.id,
			});
		}
	}
	const currentIndex = rows.findIndex(row => row.current);
	if (currentIndex > 0) {
		const [row] = rows.splice(currentIndex, 1);
		rows.unshift(row!);
	}
	return rows;
}

/** pi's ranking (`model-search.ts`): the bare id is deliberately not
 *  first, so an exact `provider/id` query ranks above proxy ids. */
function searchText(row: ModelPickerRow): string {
	return `${row.provider} ${row.provider}/${row.model} ${row.provider} ${row.model} ${row.name} ${row.providerName}`;
}

/** Narrow rows by a fuzzy query; the empty query keeps the build order. */
export function filterModelRows(rows: readonly ModelPickerRow[], query: string): ModelPickerRow[] {
	if (query === "") return [...rows];
	return fuzzyFilter([...rows], query, searchText);
}

export interface ModelPickerMeasure {
	nameWidth: number;
	contextWidth: number;
}

/** The model picker's SelectorView spec: the catalog filter, the 10-row
 *  viewport, and the three aligned columns (widths measured across the
 *  filtered set, so scrolling never reflows). */
export function modelPickerSpec(catalog: readonly AvailableProvider[], current: ModelPickerCurrent | undefined): SelectorSpec<ModelPickerRow, ModelPickerMeasure> {
	return {
		entries: buildModelRows(catalog, current),
		filter: { placeholder: "filter models…", searchText },
		pageSize: 10,
		measure: rows => ({
			nameWidth: rows.length === 0 ? 0 : Math.max(...rows.map(row => row.name.length)),
			contextWidth: rows.length === 0 ? 0 : Math.max(...rows.map(row => row.contextWindow.length)),
		}),
		row: (row, _selected, m) => {
			const mark = row.current ? accent("✓") : " ";
			const name = row.name.padEnd(m.nameWidth);
			// The context column drops out entirely when no row states one.
			const context = m.contextWidth > 0 ? `  ${warn(row.contextWindow.padStart(m.contextWidth))}` : "";
			return `${mark} ${name}${context}  ${dim(row.providerName)}`;
		},
	};
}
