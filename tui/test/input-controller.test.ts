/**
 * Ctrl+C's editor lifecycle: interrupt while running, clear an idle
 * draft, then quit once empty. Ctrl+D keeps its separate quit-when-empty
 * contract; with a draft it falls through to the editor's delete-forward.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import {
	type Editor,
	type TuiAltScreen,
	type TuiInputListener,
	type TuiInputListenerResult,
} from "@earendil-works/pi-tui";

import { InputController, type Region } from "../src/input-controller.ts";
import { applyKeybindings } from "../src/keybindings.ts";

const CTRL_C = "\x03";
const CTRL_D = "\x04";

function harness({ running = false, text = "draft", region = "editor" as Region } = {}) {
	applyKeybindings();
	let listener: TuiInputListener | undefined;
	const state = { running, text, interrupts: 0, quits: 0, renders: 0, region };
	const editor = {
		getText: () => state.text,
		setText: (next: string) => {
			state.text = next;
		},
	} as unknown as Editor;
	const tui = {
		addInputListener: (next: TuiInputListener) => {
			listener = next;
			return () => {
				listener = undefined;
			};
		},
		requestRender: () => {
			state.renders++;
		},
	} as unknown as TuiAltScreen;
	const input = new InputController({
		tui,
		editor,
		isRunning: () => state.running,
		isCardOpen: () => false,
		interrupt: () => {
			state.interrupts++;
		},
		escape: () => false,
		getRegion: () => state.region,
		setRegion: region => {
			state.region = region;
		},
		listVisible: () => false,
		transcriptScroll: () => {},
		toggleAllCollapsibles: () => {},
		onTree: () => {},
		onPasteImage: () => {},
		onQuit: () => {
			state.quits++;
		},
	});
	input.attach();
	return {
		state,
		press(data: string): TuiInputListenerResult {
			assert.ok(listener);
			return listener(data);
		},
	};
}

describe("Ctrl+C editor lifecycle", () => {
	test("a running draft interrupts without clearing", () => {
		const { state, press } = harness({ running: true, text: "keep this draft" });
		assert.deepStrictEqual(press(CTRL_C), { consume: true });
		assert.strictEqual(state.interrupts, 1);
		assert.strictEqual(state.text, "keep this draft");
		assert.strictEqual(state.quits, 0);
	});

	test("an idle multiline draft clears and repaints; the next press quits", () => {
		const { state, press } = harness({ text: "first line\nsecond line" });
		assert.deepStrictEqual(press(CTRL_C), { consume: true });
		assert.strictEqual(state.text, "");
		assert.strictEqual(state.renders, 1);
		assert.strictEqual(state.interrupts, 0);
		assert.strictEqual(state.quits, 0);

		assert.deepStrictEqual(press(CTRL_C), { consume: true });
		assert.strictEqual(state.quits, 1);
	});

	test("Ctrl+D still falls through with a draft and quits only when empty", () => {
		const { state, press } = harness({ text: "draft" });
		assert.strictEqual(press(CTRL_D), undefined);
		assert.strictEqual(state.text, "draft");
		assert.strictEqual(state.quits, 0);

		state.text = "";
		assert.deepStrictEqual(press(CTRL_D), { consume: true });
		assert.strictEqual(state.quits, 1);
	});

	test("the lifecycle remains global when the subagent list has focus", () => {
		const running = harness({ running: true, region: "list" });
		assert.deepStrictEqual(running.press(CTRL_C), { consume: true });
		assert.strictEqual(running.state.interrupts, 1);
		assert.strictEqual(running.state.text, "draft");

		const draft = harness({ region: "list" });
		assert.deepStrictEqual(draft.press(CTRL_C), { consume: true });
		assert.strictEqual(draft.state.text, "");
		assert.strictEqual(draft.state.quits, 0);
		assert.deepStrictEqual(draft.press(CTRL_C), { consume: true });
		assert.strictEqual(draft.state.quits, 1);

		const empty = harness({ text: "", region: "list" });
		assert.deepStrictEqual(empty.press(CTRL_D), { consume: true });
		assert.strictEqual(empty.state.quits, 1);
	});
});
