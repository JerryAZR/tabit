/**
 * The keyboard entry seam: the one module that turns raw input into app
 * actions, running before focused-component dispatch (so Ctrl+C is an
 * *input* that aborts a run and never a signal that kills the detached
 * backend child). Priorities, top to bottom:
 *
 *   1. interrupt a running turn        (ctrl+c / esc)
 *   2. toggle all thinking blocks      (ctrl+o)
 *   3. quit, only on an empty editor   (ctrl+c / ctrl+d — pi's rule)
 *   4. everything else falls through to the focused component
 *
 * M1 migrates these onto the engine's `Keybindings` registry (named
 * actions + user overrides); the chain stays until the action count
 * justifies the registry.
 */

import { matchesKey, type Editor, type TuiAltScreen, type TuiInputListenerResult } from "@earendil-works/pi-tui";

export interface InputControllerDeps {
	tui: TuiAltScreen;
	editor: Editor;
	isRunning: () => boolean;
	interrupt: () => void;
	/** Ctrl+O: expand/collapse every collapsible block (thinking + tool cards). */
	toggleAllCollapsibles: () => void;
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
		const { editor, isRunning, interrupt, toggleAllCollapsibles, onQuit } = this.#deps;
		if (matchesKey(data, "ctrl+c") && isRunning()) {
			interrupt();
			return { consume: true };
		}
		if (matchesKey(data, "escape") && isRunning()) {
			interrupt();
			return { consume: true };
		}
		if (matchesKey(data, "ctrl+o")) {
			toggleAllCollapsibles();
			return { consume: true };
		}
		const idleQuit = (matchesKey(data, "ctrl+c") || matchesKey(data, "ctrl+d")) && !isRunning() && editor.getText() === "";
		if (idleQuit) {
			onQuit();
			return { consume: true };
		}
		return undefined;
	}
}
