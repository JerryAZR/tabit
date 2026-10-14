/**
 * The `@` path completion provider. The engine's fuzzy file walk requires
 * the external `fd` binary (fine for pi, which locates one; we don't ship
 * one), so `@` is ours: a plain directory scan under the session cwd —
 * one level per token, dirs first with a trailing slash so completion
 * continues deeper. Everything else — slash commands, and any future
 * trigger — delegates to the wrapped CombinedAutocompleteProvider, whose
 * `applyCompletion` does the prefix replacement for both sides.
 */

import { readdir } from "node:fs/promises";
import { join } from "node:path";

import type { AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions } from "@earendil-works/pi-tui";

const AT_TOKEN = /(?:^|\s)@([^@\s]*)$/;
const MAX_ITEMS = 50;

export class AtPathCompletionProvider implements AutocompleteProvider {
	readonly #commands: AutocompleteProvider;
	readonly #base: string;

	constructor(commands: AutocompleteProvider, base: string) {
		this.#commands = commands;
		this.#base = base;
	}

	async getSuggestions(
		lines: string[],
		cursorLine: number,
		cursorCol: number,
		options: { signal: AbortSignal; force?: boolean },
	): Promise<AutocompleteSuggestions | null> {
		const before = (lines[cursorLine] ?? "").slice(0, cursorCol);
		const match = AT_TOKEN.exec(before);
		if (match === null) {
			return this.#commands.getSuggestions(lines, cursorLine, cursorCol, options);
		}
		const items = await this.#scan(match[1] ?? "", options.signal);
		if (items.length === 0) return null;
		return { items, prefix: `@${match[1] ?? ""}` };
	}

	applyCompletion(
		lines: string[],
		cursorLine: number,
		cursorCol: number,
		item: AutocompleteItem,
		prefix: string,
	): { lines: string[]; cursorLine: number; cursorCol: number } {
		return this.#commands.applyCompletion(lines, cursorLine, cursorCol, item, prefix);
	}

	/** One directory level per `@` token: `@src/fo` lists `src/`'s entries
	 *  filtered by `fo`. Dirs first (trailing slash continues the token),
	 *  then files; dotfiles only when the fragment asks for them. */
	async #scan(token: string, signal: AbortSignal): Promise<AutocompleteItem[]> {
		const cut = Math.max(token.lastIndexOf("/"), token.lastIndexOf("\\"));
		const dir = cut === -1 ? "" : token.slice(0, cut + 1);
		const fragment = cut === -1 ? token : token.slice(cut + 1);
		let names: Array<{ name: string; isDir: boolean }>;
		try {
			const entries = await readdir(join(this.#base, dir), { withFileTypes: true });
			if (signal.aborted) return [];
			names = entries.map(entry => ({ name: entry.name, isDir: entry.isDirectory() }));
		} catch {
			return []; // unreadable dir, abort, or nothing there — no dropdown
		}
		const lower = fragment.toLowerCase();
		const dirs: AutocompleteItem[] = [];
		const files: AutocompleteItem[] = [];
		for (const entry of names) {
			if (!fragment.startsWith(".") && entry.name.startsWith(".")) continue;
			if (!entry.name.toLowerCase().startsWith(lower)) continue;
			const item: AutocompleteItem = {
				value: `${dir}${entry.name}${entry.isDir ? "/" : ""}`,
				label: `${entry.name}${entry.isDir ? "/" : ""}`,
			};
			(entry.isDir ? dirs : files).push(item);
		}
		const byLabel = (a: AutocompleteItem, b: AutocompleteItem) => a.label.localeCompare(b.label);
		return [...dirs.sort(byLabel), ...files.sort(byLabel)].slice(0, MAX_ITEMS);
	}
}
