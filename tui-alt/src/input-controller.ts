/**
 * The keyboard entry seam: the one module that turns raw input into app
 * actions, running before focused-component dispatch (so Ctrl+C is an
 * *input* that aborts a run and never a signal that kills the detached
 * backend child). Actions resolve through the global keybinding registry
 * (`keybindings.ts` — named ids, user overrides from tui.toml), in the
 * same registry the editor and select lists consult. Priorities, top to
 * bottom:
 *
 *   1. interrupt a running turn        (tui.app.interrupt)
 *   2. toggle all thinking blocks      (tui.app.toggleCollapsibles)
 *   3. open the session tree           (tui.app.tree)
 *   4. paste an image as an attachment (tui.app.pasteImage)
 *   5. quit, only on an empty editor   (tui.app.quit — pi's rule)
 *   6. everything else falls through to the focused component
 */

import { getKeybindings, matchesKey, type Editor, type TuiAltScreen, type TuiInputListenerResult } from "@earendil-works/pi-tui";

export interface InputControllerDeps {
	tui: TuiAltScreen;
	editor: Editor;
	isRunning: () => boolean;
	/** Whether an interaction card is open — the card owns the keyboard
	 *  then; this listener stands down (bar Ctrl+C, the global abort
	 *  affordance) so keys reach the focused card. */
	isCardOpen: () => boolean;
	interrupt: () => void;
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
		const { editor, isRunning, isCardOpen, interrupt, toggleAllCollapsibles, onTree, onPasteImage, onQuit } = this.#deps;
		if (isCardOpen()) {
			// The card is focused and consumes everything it knows; only
			// the abort affordance preempts (a literal: the interrupt
			// action's escape leg belongs to the card while it is open).
			if (matchesKey(data, "ctrl+c") && isRunning()) {
				interrupt();
				return { consume: true };
			}
			return undefined;
		}
		const kb = getKeybindings();
		if (kb.matches(data, "tui.app.interrupt") && isRunning()) {
			interrupt();
			return { consume: true };
		}
		if (kb.matches(data, "tui.app.tree")) {
			onTree();
			return { consume: true };
		}
		if (kb.matches(data, "tui.app.toggleCollapsibles")) {
			toggleAllCollapsibles();
			return { consume: true };
		}
		if (kb.matches(data, "tui.app.pasteImage")) {
			onPasteImage();
			return { consume: true };
		}
		const idleQuit = kb.matches(data, "tui.app.quit") && !isRunning() && editor.getText() === "";
		if (idleQuit) {
			onQuit();
			return { consume: true };
		}
		return undefined;
	}
}
