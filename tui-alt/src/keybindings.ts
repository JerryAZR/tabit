/**
 * The keybinding registry (M1): named app actions over the engine's
 * `KeybindingsManager` — the global registry the editor and select lists
 * already consult — plus user overrides from `~/.tabit/tui.toml`
 * (`[keys]`), the TUI-owned TOML file (owner ruling 2026-09).
 *
 * The app's own actions join the engine's via the documented declaration
 * merge; the manager is installed globally BEFORE input flows, so every
 * `getKeybindings().matches(...)` — editor internals included — resolves
 * through one place, overrides applied.
 */

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import {
	KeybindingsManager,
	setKeybindings,
	TUI_KEYBINDINGS,
	type KeyId,
	type KeybindingDefinitions,
	type KeybindingsConfig,
} from "@earendil-works/pi-tui";

declare module "@earendil-works/pi-tui" {
	interface Keybindings {
		"tui.app.interrupt": true;
		"tui.app.toggleCollapsibles": true;
		"tui.app.quit": true;
		"tui.app.clearNote": true;
		"tui.app.tree": true;
	}
}

/** The app's own actions (engine actions keep their TUI defaults). */
export const APP_KEYBINDINGS: KeybindingDefinitions = {
	"tui.app.interrupt": {
		defaultKeys: ["escape", "ctrl+c"],
		description: "Interrupt the running turn",
	},
	"tui.app.toggleCollapsibles": {
		defaultKeys: ["ctrl+o"],
		description: "Expand or collapse every thinking block and tool card",
	},
	"tui.app.quit": {
		defaultKeys: ["ctrl+d", "ctrl+c"],
		description: "Quit (only when the editor is empty)",
	},
	"tui.app.clearNote": {
		defaultKeys: ["ctrl+u"],
		description: "Clear the note on an interaction card",
	},
	"tui.app.tree": {
		defaultKeys: ["ctrl+t"],
		description: "Open the session tree (browse and rewind)",
	},
};

/** The engine's TUI defaults, deep-copied into the mutable definitions shape. */
function allDefinitions(): KeybindingDefinitions {
	const engine = Object.fromEntries(
		Object.entries(TUI_KEYBINDINGS).map(([id, definition]) => [
			id,
			{
				defaultKeys: Array.isArray(definition.defaultKeys) ? [...definition.defaultKeys] : definition.defaultKeys,
				description: definition.description,
			},
		]),
	);
	return { ...engine, ...APP_KEYBINDINGS };
}

type AppKeybindingId = "tui.app.interrupt" | "tui.app.toggleCollapsibles" | "tui.app.quit" | "tui.app.clearNote" | "tui.app.tree";

export const APP_KEYBINDING_IDS: AppKeybindingId[] = [
	"tui.app.interrupt",
	"tui.app.toggleCollapsibles",
	"tui.app.quit",
	"tui.app.clearNote",
	"tui.app.tree",
];

/** Install the merged registry globally; returns it for display facts.
 *  The user bindings arrive as authored (raw strings from the TOML) —
 *  the cast is the validation boundary: an id or key the registry doesn't
 *  know simply never fires. */
export function applyKeybindings(userBindings: Record<string, string | string[]> = {}): KeybindingsManager {
	const manager = new KeybindingsManager(allDefinitions(), userBindings as KeybindingsConfig);
	setKeybindings(manager);
	return manager;
}

export interface ParsedTuiToml {
	/** Parsed as authored — ids the registry doesn't know simply never fire. */
	config: Record<string, string | string[]>;
	warnings: string[];
}

/**
 * Minimal TOML for the one file we own: `[keys]` with `id = "key"` or
 * `id = ["k1", "k2"]`, `#` comments. Unknown sections are ignored (room
 * for future settings); malformed lines warn and skip — user config is
 * an external input, so it fails gracefully but visibly.
 */
export function parseTuiToml(source: string): ParsedTuiToml {
	const config: Record<string, string | string[]> = {};
	const warnings: string[] = [];
	let inKeys = false;
	for (const [index, raw] of source.split("\n").entries()) {
		const line = raw.trim();
		const where = `tui.toml:${index + 1}`;
		if (line === "" || line.startsWith("#")) continue;
		if (line.startsWith("[")) {
			inKeys = line === "[keys]";
			continue;
		}
		if (!inKeys) continue;
		const eq = line.indexOf("=");
		if (eq === -1) {
			warnings.push(`${where} — not a setting, ignored: ${line}`);
			continue;
		}
		const id = line
			.slice(0, eq)
			.trim()
			.replace(/^"|"$/g, "");
		const value = line
			.slice(eq + 1)
			.trim();
		if (!id.startsWith("tui.")) {
			warnings.push(`${where} — unknown keybinding "${id}", ignored`);
			continue;
		}
		if (value.startsWith("[")) {
			if (!value.endsWith("]")) {
				warnings.push(`${where} — unterminated list, ignored`);
				continue;
			}
			const items = value
				.slice(1, -1)
				.split(",")
				.map(item => item.trim().replace(/^"|"$/g, ""))
				.filter(item => item !== "");
			config[id] = items;
		} else {
			config[id] = value.replace(/^"|"$/g, "");
		}
	}
	return { config, warnings };
}

export function tuiTomlPath(home: string): string {
	return join(home, ".tabit", "tui.toml");
}

/** Read + parse the user's overrides; a missing file is defaults and
 *  silence, an unreadable one warns. Sync: it runs once, at bind, before
 *  any input listener exists. */
export function loadTuiToml(home = homedir()): ParsedTuiToml {
	let source: string;
	try {
		source = readFileSync(tuiTomlPath(home), "utf8");
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") return { config: {}, warnings: [] };
		return { config: {}, warnings: [`tui.toml — unreadable (${(error as Error).message}); defaults in use`] };
	}
	return parseTuiToml(source);
}
