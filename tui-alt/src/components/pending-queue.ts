/**
 * The queued-steering display: the dim lines between transcript and editor
 * showing messages accepted while a run is in flight. Render-only — the
 * queue's truth is the mode's pending list, keyed by `message_queued.id`.
 */

import { Container, Text, type Component } from "@earendil-works/pi-tui";

import { marker } from "../symbols.ts";
import type { PendingMessage } from "../mode.ts";

export class PendingQueue implements Component {
	readonly #container = new Container();
	readonly #requestRender: () => void;

	constructor(requestRender: () => void) {
		this.#requestRender = requestRender;
	}

	set(pending: PendingMessage[]): void {
		this.#container.clear();
		for (const message of pending) this.#container.addChild(new Text(marker("↳", `queued: ${message.text}`)));
		this.#requestRender();
	}

	render(width: number): string[] {
		return this.#container.render(width);
	}

	invalidate(): void {
		this.#container.invalidate();
	}
}
