/**
 * The keybinding registry: the TOML subset parser, override application
 * through the engine's manager, and the load path (missing file silence,
 * unreadable warning). The manager is global — applyKeybindings installs
 * what each test needs, last write wins.
 */

import { describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { applyKeybindings, loadTuiToml, parseTuiToml, tuiTomlPath } from "../src/keybindings";

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
		expect(warnings).toEqual([]);
		expect(config["tui.app.interrupt"]).toEqual(["escape"]);
		expect(config["tui.app.quit"]).toBe("ctrl+q");
		expect(config["future.whatever"]).toBeUndefined();
	});

	test("malformed lines and unknown ids warn, never throw", () => {
		const { config, warnings } = parseTuiToml(`
[keys]
oops
"tui.app.interrupt" = ["escape"
"not.tui.binding" = "ctrl+x"
"tui.app.quit" = "ctrl+q"
`);
		expect(config["tui.app.quit"]).toBe("ctrl+q");
		expect(config["tui.app.interrupt"]).toBeUndefined();
		expect(warnings).toHaveLength(3);
	});

	test("empty source parses to defaults", () => {
		const { config, warnings } = parseTuiToml("");
		expect(config).toEqual({});
		expect(warnings).toEqual([]);
	});
});

describe("the registry", () => {
	test("defaults install; overrides replace the action's key list", () => {
		const defaults = applyKeybindings();
		expect(defaults.getKeys("tui.app.interrupt")).toEqual(["escape", "ctrl+c"]);
		expect(defaults.getKeys("tui.app.toggleCollapsibles")).toEqual(["ctrl+o"]);
		expect(defaults.getDefinition("tui.app.interrupt").description).toContain("Interrupt");

		const overridden = applyKeybindings({ "tui.app.interrupt": ["escape"] });
		expect(overridden.getKeys("tui.app.interrupt")).toEqual(["escape"]);
		// engine actions survive the merge untouched
		expect(overridden.getKeys("tui.editor.cursorUp")).toEqual(["up"]);
	});

	test("loadTuiToml: missing file is silent defaults; present file parses; unreadable warns", async () => {
		const home = mkdtempSync(join(tmpdir(), "tui-kb-"));
		try {
			const missing = await loadTuiToml(home);
			expect(missing.config).toEqual({});
			expect(missing.warnings).toEqual([]);

			mkdirSync(join(home, ".tabit"));
			writeFileSync(tuiTomlPath(home), '[keys]\n"tui.app.quit" = "ctrl+q"\n');
			const present = await loadTuiToml(home);
			expect(present.config["tui.app.quit"]).toBe("ctrl+q");
		} finally {
			rmSync(home, { recursive: true, force: true });
		}
		// a directory where tui.toml should be → unreadable (EISDIR) → warning
		const blocked = mkdtempSync(join(tmpdir(), "tui-kb-"));
		try {
			mkdirSync(join(blocked, ".tabit"));
			mkdirSync(tuiTomlPath(blocked));
			const broken = loadTuiToml(blocked);
			expect(broken.config).toEqual({});
			expect(broken.warnings).toHaveLength(1);
		} finally {
			rmSync(blocked, { recursive: true, force: true });
		}
	});
});
