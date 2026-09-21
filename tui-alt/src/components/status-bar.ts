/**
 * The status strip: the busy loader (connecting / working / compacting)
 * with its start/stop lifecycle owned here, never by callers. The strip
 * always occupies its row — a blank line when idle (the dock's breathing
 * room above the editor, ruled 2026-09), the spinner when busy — so the
 * dock never jumps when a run starts or ends. The state word lives in
 * the footer (owner ruling: one indicator, not two).
 */

import { Container, Loader, Spacer, type Component, type TuiAltScreen } from "@earendil-works/pi-tui";

import { accent, dim } from "../theme";

/** The sentinel label the mode sends when nothing is in flight. */
export const STATUS_IDLE = "idle";

export class StatusBar implements Component {
	readonly #container = new Container();
	readonly #loader: Loader;
	readonly #requestRender: () => void;
	#busy = false;

	constructor(tui: TuiAltScreen, requestRender: () => void) {
		this.#loader = new Loader(tui, accent, dim, "connecting…");
		this.#requestRender = requestRender;
		this.set("connecting…");
	}

	set(text: string): void {
		if (text === STATUS_IDLE) {
			this.#loader.stop();
			this.#busy = false;
			this.#container.clear();
			// The reserved row: a blank line, not an indicator — the state
			// word is the footer's. (An empty Text renders zero lines; the
			// Spacer is the engine's blank-line primitive.)
			this.#container.addChild(new Spacer(1));
		} else {
			if (!this.#busy) {
				this.#busy = true;
				this.#container.clear();
				this.#container.addChild(this.#loader);
			}
			this.#loader.setMessage(text);
			this.#loader.start();
		}
		this.#requestRender();
	}

	dispose(): void {
		this.#loader.stop();
	}

	render(width: number): string[] {
		return this.#container.render(width);
	}

	invalidate(): void {
		this.#container.invalidate();
	}
}
