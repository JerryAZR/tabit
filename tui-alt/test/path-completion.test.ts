/**
 * The `@` path provider, headless: real directories, real scans. The
 * engine's fuzzy walk needs the external `fd` binary — this provider is
 * the dependency-free replacement, so its contract is what the dropdown
 * shows: dirs first (trailing slash, continuation), then files, filtered
 * by fragment, `@`-prefixed replacement via the wrapped provider's
 * applyCompletion. Non-@ tokens delegate (slash commands keep working).
 */

import { describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { CombinedAutocompleteProvider } from "@earendil-works/pi-tui";

import { AtPathCompletionProvider } from "../src/path-completion";

const base = mkdtempSync(join(tmpdir(), "tui-at-"));
mkdirSync(join(base, "src"));
writeFileSync(join(base, "README.md"), "");
writeFileSync(join(base, "agenda.txt"), "");
writeFileSync(join(base, "src", "main.ts"), "");
mkdirSync(join(base, ".hidden"));

const provider = new AtPathCompletionProvider(new CombinedAutocompleteProvider([], base), base);

const suggest = async (line: string) => {
	const lines = [line];
	return provider.getSuggestions(lines, 0, line.length, { signal: new AbortController().signal });
};

describe("AtPathCompletionProvider", () => {
	test("bare @ lists the directory: dirs first with trailing slash, dotfiles hidden", async () => {
		const result = await suggest("@");
		expect(result).not.toBeNull();
		expect(result!.prefix).toBe("@");
		const labels = result!.items.map(i => i.label);
		expect(labels[0]).toBe("src/"); // dirs before files
		expect(labels).toContain("README.md");
		expect(labels).toContain("agenda.txt");
		expect(labels).not.toContain(".hidden/"); // dotfiles stay hidden unless asked
	});

	test("the fragment filters and completes deeper: @src suggests the dir to continue into", async () => {
		const result = await suggest("@sr");
		expect(result!.prefix).toBe("@sr");
		expect(result!.items).toEqual([{ value: "src/", label: "src/" }]);

		const deeper = await suggest("@src/");
		expect(deeper!.items).toEqual([{ value: "src/main.ts", label: "main.ts" }]);
	});

	test("@ fires after a space too (mid-line tokens), not only at line start", async () => {
		const result = await suggest("look at @REA");
		expect(result!.prefix).toBe("@REA");
		expect(result!.items.map(i => i.value)).toEqual(["README.md"]);
	});

	test("no @ means delegation: unknown tokens return null, slash commands still complete", async () => {
		expect(await suggest("plain words")).toBeNull();
		const commands = new AtPathCompletionProvider(
			new CombinedAutocompleteProvider([{ name: "compact", description: "d" }], base),
			base,
		);
		const slash = await commands.getSuggestions(["/comp"], 0, 5, { signal: new AbortController().signal });
		expect(slash!.items[0]!.label).toBe("compact");
	});

	test("accepting inserts the value without the @; unreadable dirs stay silent", async () => {
		const result = await suggest("@REA");
		// applyCompletion is the engine's (delegated): prefix replaced by value.
		const applied = provider.applyCompletion(["@REA"], 0, 4, result!.items[0]!, result!.prefix);
		expect(applied.lines[0]).toBe("README.md ");
		expect(await suggest("@no-such-dir/")).toEqual(null);
	});

	test("cleanup", () => {
		rmSync(base, { recursive: true, force: true });
		expect(true).toBe(true);
	});
});
