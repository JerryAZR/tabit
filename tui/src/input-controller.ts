/**
 * The keyboard entry seam: the one module that turns raw input into app
 * actions, running before focused-component dispatch (so Ctrl+C is an
 * *input* that aborts a run and never a signal that kills the detached
 * backend child). Actions resolve through the global keybinding registry
 * (`keybindings.ts` — named ids, user overrides from tui.toml), in the
 * same registry the editor and select lists consult.
 *
 * M2's REGION model (M2-DESIGN.md): focusable regions form the vertical
 * stack transcript — editor — subagent list. `tui.app.regionUp/Down`
 * (alt+↑/↓) walk the stack, skipping absent regions; within a region,
 * plain arrows are local (editor: cursor/history; list: selection).
 * Region focus is a controller-level concept for the transcript (the
 * editor keeps the toolkit focus; scroll keys are intercepted here),
 * and a toolkit focus for the list (the widget receives the keys).
 *
 * Priorities, top to bottom:
 *
 *   1. a card owns the keyboard            (bar Ctrl+C, the global abort)
 *   2. region-local laws                   (list/transcript Esc, scroll)
 *   3. Esc: abort focused run / parent walk (tui.app.escape — mode's law)
 *   4. interrupt a running turn            (tui.app.interrupt, ctrl+c)
 *   5. region navigation                   (alt+↑/↓; ↓ in an empty editor)
 *   6. toggle collapsibles / tree / paste image
 *   7. quit, only on an empty editor       (tui.app.quit — pi's rule)
 *   8. everything else falls through to the focused component
 */

import { getKeybindings, matchesKey, type Editor, type TuiAltScreen, type TuiInputListenerResult } from "@earendil-works/pi-tui";

/** The focusable regions, in stack order (M2-DESIGN.md). */
export type Region = "transcript" | "editor" | "list";

export interface InputControllerDeps {
	tui: TuiAltScreen;
	editor: Editor;
	isRunning: () => boolean;
	/** Whether an interaction card is open — the card owns the keyboard
	 *  then; this listener stands down (bar Ctrl+C, the global abort
	 *  affordance) so keys reach the focused card. */
	isCardOpen: () => boolean;
	interrupt: () => void;
	/** Esc's law (the mode's): abort the focused run, else walk to the
	 *  parent stream. Returns whether it acted — a no-op Esc falls
	 *  through to the editor (its autocomplete dismiss). */
	escape: () => boolean;
	/** The current focus region + region moves (the vanished-region law
	 *  is the caller's — the controller only ever sets what exists). */
	getRegion: () => Region;
	setRegion: (region: Region) => void;
	/** Whether the subagent list has rows — alt+↓/↓ skip an absent list. */
	listVisible: () => boolean;
	/** Scroll the transcript region (lines; negative = up). */
	transcriptScroll: (lines: number) => void;
	/** Ctrl+O: expand/collapse every collapsible block (thinking + tool cards). */
	toggleAllCollapsibles: () => void;
	/** Ctrl+T: open the session-tree card (same path as `/tree`). */
	onTree: () => void;
	/** Ctrl+V: clipboard image → temp file → attachment tag at the cursor. */
	onPasteImage: () => void;
	onQuit: () => void;
}

export class InputController {
	readonly #deps: InputControllerDeps;
	#remove: (() => void) | undefined;

	constructor(deps: InputControllerDeps) {
		this.#deps = deps;
	}

	attach(): void {
		this.#remove = this.#deps.tui.addInputListener(data => this.#handle(data));
	}

	detach(): void {
		this.#remove?.();
		this.#remove = undefined;
	}

	#handle(data: string): TuiInputListenerResult {
		const deps = this.#deps;
		const { editor, isRunning, isCardOpen } = deps;
		if (isCardOpen()) {
			// The card is focused and consumes everything it knows; only
			// the abort affordance preempts (a literal: the interrupt
			// action's escape leg belongs to the card while it is open).
			if (matchesKey(data, "ctrl+c") && isRunning()) {
				deps.interrupt();
				return { consume: true };
			}
			return undefined;
		}
		const kb = getKeybindings();
		const region = deps.getRegion();

		// --- region-local laws -------------------------------------------------
		if (region === "list") {
			// Esc / alt+↑ return to the editor without switching; everything
			// else is the widget's (arrows move, Enter focuses a stream).
			if (matchesKey(data, "escape") || kb.matches(data, "tui.app.regionUp")) {
				deps.setRegion("editor");
				return { consume: true };
			}
			return undefined;
		}
		if (region === "transcript") {
			if (matchesKey(data, "up")) {
				deps.transcriptScroll(-1);
				return { consume: true };
			}
			if (matchesKey(data, "down")) {
				deps.transcriptScroll(1);
				return { consume: true };
			}
			if (matchesKey(data, "pageUp")) {
				deps.transcriptScroll(-20);
				return { consume: true };
			}
			if (matchesKey(data, "pageDown")) {
				deps.transcriptScroll(20);
				return { consume: true };
			}
			if (matchesKey(data, "escape") || kb.matches(data, "tui.app.regionDown")) {
				deps.setRegion("editor");
				return { consume: true };
			}
			// Any other key: typing belongs to the editor — hand focus back
			// and let the key fall through to it.
			deps.setRegion("editor");
		}

		// --- the editor region's laws ------------------------------------------
		if (kb.matches(data, "tui.app.escape")) {
			// Abort the focused run / walk to the parent; a no-op Esc (root,
			// idle) falls through so the editor's autocomplete dismiss works.
			return deps.escape() ? { consume: true } : undefined;
		}
		if (kb.matches(data, "tui.app.interrupt") && isRunning()) {
			deps.interrupt();
			return { consume: true };
		}
		if (kb.matches(data, "tui.app.regionUp")) {
			deps.setRegion("transcript");
			return { consume: true };
		}
		if (kb.matches(data, "tui.app.regionDown")) {
			if (deps.listVisible()) {
				deps.setRegion("list");
				return { consume: true };
			}
			return { consume: true };
		}
		// The no-effect-↓ rule (claude-code's model, scoped to the empty
		// editor: no history nuance is possible when empty — M2-DESIGN.md).
		if (matchesKey(data, "down") && editor.getText() === "" && deps.listVisible()) {
			deps.setRegion("list");
			return { consume: true };
		}
		if (kb.matches(data, "tui.app.tree")) {
			deps.onTree();
			return { consume: true };
		}
		if (kb.matches(data, "tui.app.toggleCollapsibles")) {
			deps.toggleAllCollapsibles();
			return { consume: true };
		}
		if (kb.matches(data, "tui.app.pasteImage")) {
			deps.onPasteImage();
			return { consume: true };
		}
		const idleQuit = kb.matches(data, "tui.app.quit") && !isRunning() && editor.getText() === "";
		if (idleQuit) {
			deps.onQuit();
			return { consume: true };
		}
		return undefined;
	}
}
