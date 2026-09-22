/**
 * The interaction cards, headless: the ruled key model (cursor, space
 * toggles multi-only, enter answers, tab note mode with preserved list
 * state, ctrl+u clears) and the answer payloads the wire expects.
 * Key data below is what a terminal sends for each key.
 */

import { describe, expect, test } from "bun:test";

import { cardViewFor, type Answer } from "../src/card-view";
import { applyKeybindings } from "../src/keybindings";
import type { InteractionCard } from "../src/mode";

// The card resolves clearNote through the global registry — installed in
// production by root.bind. Install the defaults here.
applyKeybindings();

const UP = "\x1b[A";
const DOWN = "\x1b[B";
const ENTER = "\r";
const TAB = "\t";
const CTRL_U = "\x15";

const selectOne: InteractionCard = {
	id: "gate1",
	uiType: "native:select_one",
	title: "Allow bash?",
	body: "ls -la",
	options: ["Allow", "Allow always", "Block"],
	freeText: true,
};
const selectAny: InteractionCard = { id: "any1", uiType: "native:select_any", title: "pick", body: "b", options: ["a", "b", "c"], freeText: true };
const pureNote: InteractionCard = { id: "note1", uiType: "native:select_any", title: "tell me", body: "b", options: [], freeText: true };

type AnswerRecord = { selected: string[]; text: string | null };

function driven(card: InteractionCard, keys: string[]): Array<AnswerRecord> {
	const answers: AnswerRecord[] = [];
	const onAnswer: Answer = (selected, text) => answers.push({ selected, text });
	const view = cardViewFor(card, onAnswer);
	for (const key of keys) view.handleInput(key);
	return answers;
}

function typed(view: { handleInput(data: string): void }, text: string): void {
	for (const ch of text) view.handleInput(ch);
}

describe("ChoiceCardView (select_one)", () => {
	test("enter answers the option under the cursor; arrows move it", () => {
		expect(driven(selectOne, [ENTER])).toEqual([{ selected: ["Allow"], text: null }]);
		expect(driven(selectOne, [DOWN, DOWN, ENTER])).toEqual([{ selected: ["Block"], text: null }]);
	});

	test("space is inert on single-select; digits jump the cursor", () => {
		expect(driven(selectOne, [" ", DOWN, ENTER])).toEqual([{ selected: ["Allow always"], text: null }]);
		expect(driven(selectOne, ["3", ENTER])).toEqual([{ selected: ["Block"], text: null }]);
	});

	test("tab enters note mode (free_text only), typing collects, enter rides the note", () => {
		const answers: AnswerRecord[] = [];
		const view = cardViewFor(selectOne, (selected, text) => answers.push({ selected, text }));
		view.handleInput(DOWN);
		view.handleInput(DOWN);
		view.handleInput(TAB);
		typed(view, "no spaces ok");
		view.handleInput(ENTER);
		expect(answers).toEqual([{ selected: ["Block"], text: "no spaces ok" }]);
	});

	test("arrows exit note mode first, then move the cursor; ctrl+u clears the note", () => {
		const answers: AnswerRecord[] = [];
		const view = cardViewFor(selectOne, (selected, text) => answers.push({ selected, text }));
		view.handleInput(DOWN);
		view.handleInput(DOWN); // cursor on Block
		view.handleInput(TAB);
		typed(view, "reason ");
		view.handleInput(UP); // exits note mode AND moves the cursor: Block → Allow always
		view.handleInput(ENTER);
		expect(answers).toEqual([{ selected: ["Allow always"], text: "reason " }]);

		const cleared: AnswerRecord[] = [];
		const view2 = cardViewFor(selectOne, (selected, text) => cleared.push({ selected, text }));
		view2.handleInput(TAB);
		typed(view2, "draft");
		view2.handleInput(CTRL_U);
		typed(view2, "final");
		view2.handleInput(ENTER);
		expect(cleared).toEqual([{ selected: ["Allow"], text: "final" }]);
	});

	test("tab without free_text does nothing", () => {
		const noNote: InteractionCard = { ...selectOne, freeText: false };
		expect(driven(noNote, [TAB, "x", ENTER])).toEqual([{ selected: ["Allow"], text: null }]);
	});
});

describe("ChoiceCardView (select_any)", () => {
	test("space toggles the set; enter answers the toggled-on set ignoring the cursor", () => {
		expect(driven(selectAny, [" ", DOWN, DOWN, " ", ENTER])).toEqual([{ selected: ["a", "c"], text: null }]);
		expect(driven(selectAny, [" ", " ", ENTER])).toEqual([{ selected: [], text: null }]);
	});
});

describe("NoteCardView (the zero-option free-text ask)", () => {
	test("typing is the answer; enter sends it, ctrl+u clears", () => {
		expect(driven(pureNote, ["h", "i", ENTER])).toEqual([{ selected: [], text: "hi" }]);
		expect(driven(pureNote, ["h", "i", CTRL_U, ENTER])).toEqual([{ selected: [], text: null }]);
	});
});

describe("rendering", () => {
	test("the choice card draws cursor, toggle marks, and the note line only in note mode", () => {
		const view = cardViewFor(selectAny, () => {});
		view.handleInput(" ");
		view.handleInput(DOWN);
		const idle = view.render(60).join("\n");
		expect(idle).toContain("[×] 1. a");
		expect(idle).toContain("❯ ");
		expect(idle).not.toContain("note:");

		view.handleInput(TAB);
		expect(view.render(60).join("\n")).toContain("note:");
	});
});
