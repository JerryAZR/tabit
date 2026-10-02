/**
 * The keybinding registry: the TOML subset parser, override application
 * through the engine's manager, and the load path (missing file silence,
 * unreadable warning). The manager is global — applyKeybindings installs
 * what each test needs, last write wins.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { applyKeybindings, loadTuiToml, parseTuiToml, tuiTomlPath } from "../src/keybindings.ts";

describe("tui.toml parsing", () => {
	test("overrides: strings and arrays under [keys]; comments and other sections ignored", () => {
		const { config, warnings } = parseTuiToml(`
# tui settings
[keys]
"tui.app.interrupt" = ["escape"]
"tui.app.quit" = "ctrl+q"

[future]
whatever = "ignored"
`);
		assert.deepStrictEqual(warnings, []);
		assert.deepStrictEqual(config["tui.app.interrupt"], ["escape"]);
		assert.strictEqual(config["tui.app.quit"], "ctrl+q");
		assert.strictEqual(config["future.whatever"], undefined);
	});

	test("malformed lines and unknown ids warn, never throw", () => {
		const { config, warnings } = parseTuiToml(`
[keys]
oops
"tui.app.interrupt" = ["escape"
"not.tui.binding" = "ctrl+x"
"tui.app.quit" = "ctrl+q"
`);
		assert.strictEqual(config["tui.app.quit"], "ctrl+q");
		assert.strictEqual(config["tui.app.interrupt"], undefined);
		assert.strictEqual(warnings.length, 3);
	});

	test("empty source parses to defaults", () => {
		const { config, warnings } = parseTuiToml("");
		assert.deepStrictEqual(config, {});
		assert.deepStrictEqual(warnings, []);
	});
});

describe("the registry", () => {
	test("defaults install; overrides replace the action's key list", () => {
		const defaults = applyKeybindings();
		assert.deepStrictEqual(defaults.getKeys("tui.app.interrupt"), ["escape", "ctrl+c"]);
		assert.deepStrictEqual(defaults.getKeys("tui.app.toggleCollapsibles"), ["ctrl+o"]);
		assert.ok(defaults.getDefinition("tui.app.interrupt").description?.includes("Interrupt"));

		const overridden = applyKeybindings({ "tui.app.interrupt": ["escape"] });
		assert.deepStrictEqual(overridden.getKeys("tui.app.interrupt"), ["escape"]);
		// engine actions survive the merge untouched
		assert.deepStrictEqual(overridden.getKeys("tui.editor.cursorUp"), ["up"]);
	});

	test("loadTuiToml: missing file is silent defaults; present file parses; unreadable warns", async () => {
		const home = mkdtempSync(join(tmpdir(), "tui-kb-"));
		try {
			const missing = await loadTuiToml(home);
			assert.deepStrictEqual(missing.config, {});
			assert.deepStrictEqual(missing.warnings, []);

			mkdirSync(join(home, ".tabit"));
			writeFileSync(tuiTomlPath(home), '[keys]\n"tui.app.quit" = "ctrl+q"\n');
			const present = await loadTuiToml(home);
			assert.strictEqual(present.config["tui.app.quit"], "ctrl+q");
		} finally {
			rmSync(home, { recursive: true, force: true });
		}
		// a directory where tui.toml should be → unreadable (EISDIR) → warning
		const blocked = mkdtempSync(join(tmpdir(), "tui-kb-"));
		try {
			mkdirSync(join(blocked, ".tabit"));
			mkdirSync(tuiTomlPath(blocked));
			const broken = loadTuiToml(blocked);
			assert.deepStrictEqual(broken.config, {});
			assert.strictEqual(broken.warnings.length, 1);
		} finally {
			rmSync(blocked, { recursive: true, force: true });
		}
	});
});
