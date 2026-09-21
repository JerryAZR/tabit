/**
 * Base class for every transcript block. A block is an engine `Component`
 * that knows which turn created it — `turn_retried` removes a turn's
 * blocks as a group via this identity — and how to ask the engine for a
 * repaint after mutating itself (the engine paints only on request;
 * blocks own that call for their own updates).
 *
 * Inheritance is deliberately shallow: identity + repaint live here,
 * rendering and accumulation live in each block. The engine's Component
 * is a structural interface, so blocks are substitutable without further
 * hierarchy.
 */

import type { Component } from "@earendil-works/pi-tui";

export abstract class TranscriptBlock implements Component {
	readonly turnId: string;
	readonly #requestRender: () => void;

	protected constructor(turnId: string, requestRender: () => void) {
		this.turnId = turnId;
		this.#requestRender = requestRender;
	}

	protected touch(): void {
		this.#requestRender();
	}

	/**
	 * The component the transcript actually hosts. Default: this block.
	 * Overrides wrap themselves (mouse regions, focus shims) so the
	 * transcript never needs to know.
	 */
	asComponent(): Component {
		return this;
	}

	abstract render(width: number): string[];
	invalidate(): void {}
}
