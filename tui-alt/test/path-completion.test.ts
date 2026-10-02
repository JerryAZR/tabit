/**
 * The `@` path provider, headless: real directories, real scans. The
 * engine's fuzzy walk needs the external `fd` binary — this provider is
 * the dependency-free replacement, so its contract is what the dropdown
 * shows: dirs first (trailing slash, continuation), then files, filtered
 * by fragment, `@`-prefixed replacement via the wrapped provider's
 * applyCompletion. Non-@ tokens delegate (slash commands keep working).
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { CombinedAutocompleteProvider } from "@earendil-works/pi-tui";

import { AtPathCompletionProvider } from "../src/path-completion.ts";

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
		assert.notStrictEqual(result, null);
		assert.strictEqual(result!.prefix, "@");
		const labels = result!.items.map(i => i.label);
		assert.strictEqual(labels[0], "src/"); // dirs before files
		assert.ok(labels.includes("README.md"));
		assert.ok(labels.includes("agenda.txt"));
		assert.ok(!labels.includes(".hidden/")); // dotfiles stay hidden unless asked
	});

	test("the fragment filters and completes deeper: @src suggests the dir to continue into", async () => {
		const result = await suggest("@sr");
		assert.strictEqual(result!.prefix, "@sr");
		assert.deepStrictEqual(result!.items, [{ value: "src/", label: "src/" }]);

		const deeper = await suggest("@src/");
		assert.deepStrictEqual(deeper!.items, [{ value: "src/main.ts", label: "main.ts" }]);
	});

	test("@ fires after a space too (mid-line tokens), not only at line start", async () => {
		const result = await suggest("look at @REA");
		assert.strictEqual(result!.prefix, "@REA");
		assert.deepStrictEqual(result!.items.map(i => i.value), ["README.md"]);
	});

	test("no @ means delegation: unknown tokens return null, slash commands still complete", async () => {
		assert.strictEqual(await suggest("plain words"), null);
		const commands = new AtPathCompletionProvider(
			new CombinedAutocompleteProvider([{ name: "compact", description: "d" }], base),
			base,
		);
		const slash = await commands.getSuggestions(["/comp"], 0, 5, { signal: new AbortController().signal });
		assert.strictEqual(slash!.items[0]!.label, "compact");
	});

	test("accepting inserts the value without the @; unreadable dirs stay silent", async () => {
		const result = await suggest("@REA");
		// applyCompletion is the engine's (delegated): prefix replaced by value.
		const applied = provider.applyCompletion(["@REA"], 0, 4, result!.items[0]!, result!.prefix);
		assert.strictEqual(applied.lines[0], "README.md ");
		assert.deepStrictEqual(await suggest("@no-such-dir/"), null);
	});

	test("cleanup", () => {
		rmSync(base, { recursive: true, force: true });
		assert.strictEqual(true, true);
	});
});
