/**
 * Interaction cards, two implementations chosen once at construction
 * (owner ruling 2026-09 — the split lives at entry, not as branches in a
 * shared class):
 *
 * - `ChoiceCardView` — options on screen, a cursor, and a toggled set.
 *   ↑/↓ move the cursor (exiting note mode first); space toggles, only
 *   meaningful for select_any; enter answers — select_one answers the
 *   option under the cursor, select_any answers the toggled-on set —
 *   plus the note when non-empty; tab switches to note mode and back,
 *   preserving cursor and toggles; ctrl+u (tui.app.clearNote) clears the
 *   note. No dismissal: unanswered cards fail closed at the run terminal.
 * - `NoteCardView` — the zero-option free-text ask: the card *is* the
 *   note; enter sends `{selected: [], text}`.
 *
 * While a card is open the input listener stands down (bar Ctrl+C), so
 * the card owns the keyboard.
 */

import { getKeybindings, matchesKey, type Component } from "@earendil-works/pi-tui";

import { dim } from "./theme.ts";

import type { InteractionCard } from "./mode.ts";

export type Answer = (selected: string[], text: string | null) => void;

const isPrintable = (data: string): boolean => data.length === 1 && data >= " " && data !== "\x7f";

/** What the dock hosts: focusable, keyboard-receiving, self-rendering. */
export interface InteractionCardView extends Component {
	focusable: boolean;
	handleInput(data: string): void;
}

export function cardViewFor(card: InteractionCard, onAnswer: Answer): InteractionCardView {
	return card.options.length > 0 ? new ChoiceCardView(card, onAnswer) : new NoteCardView(card, onAnswer);
}

class ChoiceCardView implements Component {
	readonly focusable = true;
	#cursor = 0;
	readonly #toggled = new Set<number>();
	#note = "";
	#noteMode = false;
	readonly #card: InteractionCard;
	readonly #onAnswer: Answer;

	constructor(card: InteractionCard, onAnswer: Answer) {
		this.#card = card;
		this.#onAnswer = onAnswer;
	}

	handleInput(data: string): void {
		const kb = getKeybindings();
		// ↑/↓ belong to the list in both modes: in note mode they exit
		// note editing first, then move (owner ruling).
		if (matchesKey(data, "up")) {
			this.#noteMode = false;
			this.#cursor = Math.max(0, this.#cursor - 1);
			return;
		}
		if (matchesKey(data, "down")) {
			this.#noteMode = false;
			this.#cursor = Math.min(this.#card.options.length - 1, this.#cursor + 1);
			return;
		}
		if (this.#noteMode) {
			if (kb.matches(data, "tui.app.clearNote")) {
				this.#note = "";
				return;
			}
			if (kb.matches(data, "tui.input.tab") || matchesKey(data, "escape")) {
				this.#noteMode = false; // back to the list, cursor and toggles preserved
				return;
			}
			if (kb.matches(data, "tui.input.submit")) {
				this.answer();
				return;
			}
			if (data === "\x7f") {
				this.#note = this.#note.slice(0, -1);
				return;
			}
			if (isPrintable(data)) {
				this.#note += data;
			}
			return;
		}
		// List mode.
		if (kb.matches(data, "tui.input.submit")) {
			this.answer();
			return;
		}
		if ((kb.matches(data, "tui.input.tab") || matchesKey(data, "escape")) && this.#card.freeText) {
			this.#noteMode = true; // cursor and toggles preserved
			return;
		}
		if (this.#card.freeText && kb.matches(data, "tui.app.clearNote")) {
			this.#note = "";
			return;
		}
		if (data === " " && this.#card.uiType === "native:select_any") {
			this.#toggled.has(this.#cursor) ? this.#toggled.delete(this.#cursor) : this.#toggled.add(this.#cursor);
			return;
		}
		const digit = Number(data);
		if (Number.isInteger(digit) && digit >= 1 && digit <= this.#card.options.length) {
			this.#cursor = digit - 1;
		}
	}

	invalidate(): void {}

	private answer(): void {
		const selected =
			this.#card.uiType === "native:select_one"
				? [this.#card.options[this.#cursor]!]
				: [...this.#toggled].sort((a, b) => a - b).map(index => this.#card.options[index]!);
		this.#onAnswer(selected, this.#note === "" ? null : this.#note);
	}

	render(width: number): string[] {
		const rule = dim("─".repeat(Math.max(1, width)));
		const lines: string[] = [rule, ` ${this.#card.title}`];
		for (const bodyLine of this.#card.body.split("\n")) lines.push(` ${bodyLine}`);
		// pi's SelectList row language: the cursor prefix is the selection
		// indicator — no toggle boxes on single-select (space is inert
		// there; the toggled set exists only for select_any).
		const multi = this.#card.uiType === "native:select_any";
		this.#card.options.forEach((option, index) => {
			const cursor = index === this.#cursor ? "❯ " : "  ";
			const mark = multi ? (this.#toggled.has(index) ? "[×] " : "[ ] ") : "";
			lines.push(` ${cursor}${mark}${index + 1}. ${option}`);
		});
		if (this.#noteMode || this.#note !== "") {
			lines.push(` note: ${this.#note}${this.#noteMode ? "▏" : ""}`);
			lines.push(" enter sends · esc/tab back to choices · ctrl+u clears");
		}
		lines.push(rule);
		return lines;
	}
}

class NoteCardView implements Component {
	readonly focusable = true;
	#note = "";
	readonly #card: InteractionCard;
	readonly #onAnswer: Answer;

	constructor(card: InteractionCard, onAnswer: Answer) {
		this.#card = card;
		this.#onAnswer = onAnswer;
	}

	handleInput(data: string): void {
		const kb = getKeybindings();
		if (kb.matches(data, "tui.input.submit")) {
			this.#onAnswer([], this.#note === "" ? null : this.#note);
			return;
		}
		if (kb.matches(data, "tui.app.clearNote")) {
			this.#note = "";
			return;
		}
		if (data === "\x7f") {
			this.#note = this.#note.slice(0, -1);
			return;
		}
		if (isPrintable(data)) {
			this.#note += data;
		}
	}

	render(width: number): string[] {
		const rule = dim("─".repeat(Math.max(1, width)));
		const lines: string[] = [rule, ` ${this.#card.title}`];
		for (const bodyLine of this.#card.body.split("\n")) lines.push(` ${bodyLine}`);
		lines.push(` ${this.#note}▏`);
		lines.push(" enter sends · ctrl+u clears");
		lines.push(rule);
		return lines;
	}

	invalidate(): void {}
}
