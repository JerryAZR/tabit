/**
 * The footer container: one line, the registered badges joined by the
 * separator it owns. It knows order, separation, and lifecycle — nothing
 * about any segment's content (each badge file owns that). Facts flow in
 * as the mode's pushed snapshot; a repaint rides every push (retained
 * mode: no push, no paint — which is why self-sourcing badges call
 * `ctx.requestRender()` themselves).
 */

import { Text, type Component } from "@earendil-works/pi-tui";

import { dim } from "../theme";
import type { FooterFacts } from "../mode";
import { FOOTER_BADGES, type FooterBadge } from "./registry";

const SEPARATOR = dim("  ·  ");

export class FooterBar implements Component {
	readonly #line = new Text("");
	readonly #badges: FooterBadge[];
	readonly #requestRender: () => void;

	constructor(requestRender: () => void) {
		this.#requestRender = requestRender;
		this.#badges = FOOTER_BADGES.map(factory => factory({ requestRender }));
	}

	set(facts: FooterFacts): void {
		const segments = this.#badges.map(badge => badge.render(facts)).filter(s => s !== undefined);
		this.#line.setText(segments.join(SEPARATOR));
		this.#requestRender();
	}

	dispose(): void {
		for (const badge of this.#badges) badge.dispose?.();
	}

	render(width: number): string[] {
		return this.#line.render(width);
	}

	invalidate(): void {
		this.#line.invalidate();
	}
}
