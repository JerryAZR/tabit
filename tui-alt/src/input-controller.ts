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
 *   3. quit, only on an empty editor   (tui.app.quit — pi's rule)
 *   4. everything else falls through to the focused component
 */

import { getKeybindings, type Editor, type TuiAltScreen, type TuiInputListenerResult } from "@earendil-works/pi-tui";

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
		const kb = getKeybindings();
		if (kb.matches(data, "tui.app.interrupt") && isRunning()) {
			interrupt();
			return { consume: true };
		}
		if (kb.matches(data, "tui.app.toggleCollapsibles")) {
			toggleAllCollapsibles();
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
