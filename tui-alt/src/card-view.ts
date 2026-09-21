/**
 * One interaction card, rendered in the dock's card slot and focused while
 * open (the editor-replacement pattern: focus returns to the editor on
 * close). Digits 1..n answer; there is deliberately no dismissal — the
 * backend owns policy, and an unanswered card fails closed at the run
 * terminal.
 */

import { Container } from "@earendil-works/pi-tui";

import type { InteractionCard } from "./mode";

export class CardView extends Container {
	/** Marks this component focusable for the engine's focus system. */
	readonly focusable = true;

	constructor(
		private readonly card: InteractionCard,
		private readonly onAnswer: (optionIndex: number) => void,
	) {
		super();
	}

	handleInput(data: string): void {
		for (let index = 0; index < this.card.options.length; index++) {
			if (data === String(index + 1)) {
				this.onAnswer(index);
				return;
			}
		}
	}

	override render(width: number): string[] {
		const lines: string[] = [`┌─  ${this.card.title}`];
		for (const bodyLine of this.card.body.split("\n")) lines.push(`│  ${bodyLine}`);
		this.card.options.forEach((option, index) => lines.push(`│  ${index + 1}. ${option}`));
		if (this.card.options.length === 0 && this.card.freeText) {
			lines.push("│  (free-text answering lands in M1 — this card cannot be answered yet)");
		}
		lines.push(`└${"─".repeat(Math.max(4, width - 2))}`);
		return lines;
	}
}
