/**
 * The interaction cards, headless: the ruled key model (cursor, space
 * toggles multi-only, enter answers, tab note mode with preserved list
 * state, ctrl+u clears) and the answer payloads the wire expects.
 * Key data below is what a terminal sends for each key.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { cardViewFor, type Answer } from "../src/card-view.ts";
import { applyKeybindings } from "../src/keybindings.ts";
import type { InteractionCard } from "../src/mode.ts";

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
		assert.deepStrictEqual(driven(selectOne, [ENTER]), [{ selected: ["Allow"], text: null }]);
		assert.deepStrictEqual(driven(selectOne, [DOWN, DOWN, ENTER]), [{ selected: ["Block"], text: null }]);
	});

	test("space is inert on single-select; digits jump the cursor", () => {
		assert.deepStrictEqual(driven(selectOne, [" ", DOWN, ENTER]), [{ selected: ["Allow always"], text: null }]);
		assert.deepStrictEqual(driven(selectOne, ["3", ENTER]), [{ selected: ["Block"], text: null }]);
	});

	test("tab enters note mode (free_text only), typing collects, enter rides the note", () => {
		const answers: AnswerRecord[] = [];
		const view = cardViewFor(selectOne, (selected, text) => answers.push({ selected, text }));
		view.handleInput(DOWN);
		view.handleInput(DOWN);
		view.handleInput(TAB);
		typed(view, "no spaces ok");
		view.handleInput(ENTER);
		assert.deepStrictEqual(answers, [{ selected: ["Block"], text: "no spaces ok" }]);
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
		assert.deepStrictEqual(answers, [{ selected: ["Allow always"], text: "reason " }]);

		const cleared: AnswerRecord[] = [];
		const view2 = cardViewFor(selectOne, (selected, text) => cleared.push({ selected, text }));
		view2.handleInput(TAB);
		typed(view2, "draft");
		view2.handleInput(CTRL_U);
		typed(view2, "final");
		view2.handleInput(ENTER);
		assert.deepStrictEqual(cleared, [{ selected: ["Allow"], text: "final" }]);
	});

	test("tab without free_text does nothing", () => {
		const noNote: InteractionCard = { ...selectOne, freeText: false };
		assert.deepStrictEqual(driven(noNote, [TAB, "x", ENTER]), [{ selected: ["Allow"], text: null }]);
	});
});

describe("ChoiceCardView (select_any)", () => {
	test("space toggles the set; enter answers the toggled-on set ignoring the cursor", () => {
		assert.deepStrictEqual(driven(selectAny, [" ", DOWN, DOWN, " ", ENTER]), [{ selected: ["a", "c"], text: null }]);
		assert.deepStrictEqual(driven(selectAny, [" ", " ", ENTER]), [{ selected: [], text: null }]);
	});
});

describe("NoteCardView (the zero-option free-text ask)", () => {
	test("typing is the answer; enter sends it, ctrl+u clears", () => {
		assert.deepStrictEqual(driven(pureNote, ["h", "i", ENTER]), [{ selected: [], text: "hi" }]);
		assert.deepStrictEqual(driven(pureNote, ["h", "i", CTRL_U, ENTER]), [{ selected: [], text: null }]);
	});
});

describe("rendering", () => {
	test("the choice card draws cursor, toggle marks, and the note line only in note mode", () => {
		const view = cardViewFor(selectAny, () => {});
		view.handleInput(" ");
		view.handleInput(DOWN);
		const idle = view.render(60).join("\n");
		assert.ok(idle.includes("[×] 1. a"));
		assert.ok(idle.includes("❯ "));
		assert.ok(!idle.includes("note:"));

		view.handleInput(TAB);
		assert.ok(view.render(60).join("\n").includes("note:"));
	});
});
