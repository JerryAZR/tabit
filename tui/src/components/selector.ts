/**
 * The docked list selector — the ONE home for the machinery every
 * slash-command picker shares: a wrapping ↑/↓ cursor, an optional fuzzy
 * filter line (catalogs), an optional paged viewport (long lists), the
 * rule/title/footer chrome, and enter-selects/esc-closes. The tree card
 * is not a selector (a tree view, not a list); everything else that
 * picks from a list mounts here.
 *
 * What a consumer supplies is only its presentation (`SelectorSpec`):
 * the entries (pre-built, any current row already marked and pinned
 * first by the builder), an optional filter, an optional page size, and
 * a row renderer. `measure` runs once per render over the FILTERED set,
 * so column alignment never reflows while scrolling — a one-column row
 * is just the degenerate case.
 *
 * What happens AFTER the pick is none of this component's business:
 * `onSelect(entry)` is the consumer's continuation — send a command,
 * or open a custom UI (the login key form). Esc reports `onClose`.
 */

import { fuzzyFilter, getKeybindings, Input, truncateToWidth, type Component } from "@earendil-works/pi-tui";

import { accent, dim } from "../theme.ts";

export interface SelectorSpec<T, M = void> {
	/** Pre-built rows: the current entry (if any) already marked and
	 *  pinned first by the consumer's builder, so the cursor homes at 0. */
	entries: T[];
	/** The chrome's title line (e.g. " thinking level"); catalogs use the
	 *  filter's placeholder instead. */
	title?: string;
	/** The fuzzy search line; absent for dials and small target lists. */
	filter?: { placeholder: string; searchText: (entry: T) => string };
	/** The bounded viewport; ←/→ page-clamp. Absent renders every row. */
	pageSize?: number;
	/** Cross-row presentation facts (column widths) over the filtered set. */
	measure?: (entries: readonly T[]) => M;
	/** One row's content AFTER the core's cursor prefix. */
	row: (entry: T, selected: boolean, measure: M) => string;
}

export interface SelectorHooks<T> {
	onSelect(entry: T): void;
	onClose(): void;
}

export class SelectorView<T, M = void> implements Component {
	readonly focusable = true;
	readonly #spec: SelectorSpec<T, M>;
	readonly #hooks: SelectorHooks<T>;
	readonly #requestRender: () => void;
	readonly #input: Input | undefined;
	#rows: T[];
	#cursor = 0;

	constructor(spec: SelectorSpec<T, M>, hooks: SelectorHooks<T>, requestRender: () => void) {
		this.#spec = spec;
		this.#hooks = hooks;
		this.#requestRender = requestRender;
		this.#rows = spec.entries;
		if (spec.filter !== undefined) {
			this.#input = new Input({ placeholder: spec.filter.placeholder });
			this.#input.onSubmit = () => this.#confirm();
			this.#input.onEscape = () => this.#hooks.onClose();
		}
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
		const page = this.#spec.pageSize;
		if (page !== undefined && kb.matches(data, "tui.editor.cursorLeft")) {
			this.#move(-page);
			return;
		}
		if (page !== undefined && kb.matches(data, "tui.editor.cursorRight")) {
			this.#move(page);
			return;
		}
		if (this.#input !== undefined) {
			// Text editing delegates to the Input (enter/escape come back as
			// its onSubmit/onEscape); a changed query re-filters and re-homes.
			const before = this.#input.getValue();
			this.#input.handleInput(data);
			if (this.#input.getValue() !== before) {
				const query = this.#input.getValue().trim();
				this.#rows = query === "" ? this.#spec.entries : fuzzyFilter([...this.#spec.entries], query, this.#spec.filter!.searchText);
				this.#cursor = 0;
			}
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.select.cancel")) {
			this.#hooks.onClose();
			return;
		}
		if (kb.matches(data, "tui.select.confirm")) this.#confirm();
	}

	#move(delta: number): void {
		const total = this.#rows.length;
		if (total === 0) return;
		// ↑/↓ wrap (pi's selector rule); ←/→ page-clamp.
		if (delta === -1) this.#cursor = this.#cursor === 0 ? total - 1 : this.#cursor - 1;
		else if (delta === 1) this.#cursor = this.#cursor === total - 1 ? 0 : this.#cursor + 1;
		else this.#cursor = Math.min(total - 1, Math.max(0, this.#cursor + delta));
		this.#requestRender();
	}

	#confirm(): void {
		const entry = this.#rows[this.#cursor];
		if (entry !== undefined) this.#hooks.onSelect(entry);
	}

	render(width: number): string[] {
		const rule = dim("─".repeat(Math.max(1, width)));
		const lines: string[] = [rule];
		if (this.#spec.title !== undefined) lines.push(dim(this.#spec.title));
		if (this.#input !== undefined) lines.push(...this.#input.render(width));
		if (this.#rows.length === 0) {
			lines.push(dim(" no matches"));
		} else {
			this.#cursor = Math.min(this.#cursor, this.#rows.length - 1);
			const measure = this.#spec.measure?.(this.#rows) as M;
			const page = this.#spec.pageSize ?? this.#rows.length;
			const startIndex = Math.max(0, Math.min(this.#cursor - Math.floor(page / 2), this.#rows.length - page));
			const endIndex = Math.min(startIndex + page, this.#rows.length);
			for (let index = startIndex; index < endIndex; index++) {
				const cursor = index === this.#cursor ? accent("❯") : " ";
				lines.push(truncateToWidth(` ${cursor} ${this.#spec.row(this.#rows[index]!, index === this.#cursor, measure)}`, width));
			}
			// The counter is a catalog signal (a filtered set's size); dials
			// and small target lists read without it.
			const counter = this.#spec.filter !== undefined ? `(${this.#cursor + 1}/${this.#rows.length}) · ` : "";
			lines.push(dim(` ${counter}enter selects · esc closes`));
		}
		lines.push(rule);
		return lines;
	}

	invalidate(): void {}
}
