/**
 * The `/model` picker: pi's selector pattern (a search line over sparse
 * rows, esc cancels, enter selects) hosted in the dock's card slot like
 * the tree card. Rows are the owner-ruled three aligned columns —
 * name | context window | provider — with the current register pinned
 * first under a `✓`; a register outside the catalog simply has no `✓`
 * (owner ruling: the footer carries the current state, the picker needn't
 * synthesize one). Picking sends the `model` command with no
 * thinking_level (null is always legal — the provider/model default).
 *
 * The rows builder and filter are pure and pi-tui-free for tests; the
 * view wraps pi-tui's `Input` for the search line (kill ring, cursor
 * movement, paste) and intercepts the navigation keys before delegating.
 */

import { fuzzyFilter, getKeybindings, Input, truncateToWidth, type Component } from "@earendil-works/pi-tui";

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

export interface ModelPickerHooks {
	onSelect(provider: string, model: string): void;
	onClose(): void;
}

/** Bounded viewport — the dock card must not grow with the catalog. */
const MAX_VISIBLE_ROWS = 10;

export class ModelPickerView implements Component {
	readonly focusable = true;
	readonly #input = new Input({ placeholder: "filter models…" });
	readonly #allRows: ModelPickerRow[];
	#rows: ModelPickerRow[];
	#cursor = 0;
	readonly #hooks: ModelPickerHooks;
	readonly #requestRender: () => void;

	constructor(catalog: readonly AvailableProvider[], current: ModelPickerCurrent | undefined, hooks: ModelPickerHooks, requestRender: () => void) {
		this.#allRows = buildModelRows(catalog, current);
		this.#rows = this.#allRows;
		this.#hooks = hooks;
		this.#requestRender = requestRender;
		this.#input.onSubmit = () => this.#confirm();
		this.#input.onEscape = () => this.#hooks.onClose();
	}

	handleInput(data: string): void {
		const kb = getKeybindings();
		if (kb.matches(data, "tui.select.up")) {
			this.#move(-1);
			return;
		}
		if (kb.matches(data, "tui.select.down")) {
			this.#move(1);
			return;
		}
		if (kb.matches(data, "tui.editor.cursorLeft")) {
			this.#move(-MAX_VISIBLE_ROWS);
			return;
		}
		if (kb.matches(data, "tui.editor.cursorRight")) {
			this.#move(MAX_VISIBLE_ROWS);
			return;
		}
		// Text editing delegates to the Input (enter/escape come back as its
		// onSubmit/onEscape); a changed query re-filters and re-homes.
		const before = this.#input.getValue();
		this.#input.handleInput(data);
		if (this.#input.getValue() !== before) {
			this.#rows = filterModelRows(this.#allRows, this.#input.getValue().trim());
			this.#cursor = 0;
		}
		this.#requestRender();
	}

	#move(delta: number): void {
		const total = this.#rows.length;
		if (total === 0) return;
		// ↑/↓ wrap (pi's selector rule, the tree card's too); ←/→ page-clamp.
		if (delta === -1) this.#cursor = this.#cursor === 0 ? total - 1 : this.#cursor - 1;
		else if (delta === 1) this.#cursor = this.#cursor === total - 1 ? 0 : this.#cursor + 1;
		else this.#cursor = Math.min(total - 1, Math.max(0, this.#cursor + delta));
		this.#requestRender();
	}

	#confirm(): void {
		const row = this.#rows[this.#cursor];
		if (row !== undefined) this.#hooks.onSelect(row.provider, row.model);
	}

	invalidate(): void {}

	render(width: number): string[] {
		const rule = dim("─".repeat(Math.max(1, width)));
		const lines: string[] = [rule, ...this.#input.render(width)];
		if (this.#rows.length === 0) {
			lines.push(dim(" no matches"));
		} else {
			this.#cursor = Math.min(this.#cursor, this.#rows.length - 1);
			// Columns align across the filtered set (not just the window), so
			// scrolling never reflows.
			const nameWidth = Math.max(...this.#rows.map(row => row.name.length));
			const contextWidth = Math.max(...this.#rows.map(row => row.contextWindow.length));
			const startIndex = Math.max(
				0,
				Math.min(this.#cursor - Math.floor(MAX_VISIBLE_ROWS / 2), this.#rows.length - MAX_VISIBLE_ROWS),
			);
			const endIndex = Math.min(startIndex + MAX_VISIBLE_ROWS, this.#rows.length);
			for (let index = startIndex; index < endIndex; index++) {
				lines.push(truncateToWidth(this.#row(this.#rows[index]!, index === this.#cursor, nameWidth, contextWidth), width));
			}
			lines.push(dim(` (${this.#cursor + 1}/${this.#rows.length}) · enter selects · esc closes`));
		}
		lines.push(rule);
		return lines;
	}

	#row(row: ModelPickerRow, selected: boolean, nameWidth: number, contextWidth: number): string {
		const cursor = selected ? accent("❯") : " ";
		const mark = row.current ? accent("✓") : " ";
		const name = row.name.padEnd(nameWidth);
		// The context column drops out entirely when no row states one.
		const context = contextWidth > 0 ? `  ${warn(row.contextWindow.padStart(contextWidth))}` : "";
		return ` ${cursor} ${mark} ${name}${context}  ${dim(row.providerName)}`;
	}
}
