/**
 * The `/login` + `/logout` surfaces, headless: the consumer law made
 * literal (login lists `auth: "none"`, logout lists `auth: "stored"`),
 * the two continuation kinds (logout's pick IS the command; login's pick
 * opens the key form — the root's continuation, emulated by the rig),
 * and the copy discipline — the key reaches the hook and is never echoed.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { AuthKeyCard, authPickSpec, type AuthCardKind } from "../src/auth-card.ts";
import { SelectorView } from "../src/components/selector.ts";
import { applyKeybindings } from "../src/keybindings.ts";
import type { ProviderStatus } from "../src/protocol.ts";

// The selector resolves select actions through the global registry —
// installed in production by root.bind. Install the defaults here.
applyKeybindings();

const UP = "\x1b[A";
const DOWN = "\x1b[B";
const ENTER = "\r";
const ESCAPE = "\x1b";
const ANSI = /\x1b\[[0-9;]*m/g;

const statuses: ProviderStatus[] = [
	{ id: "anthropic", auth: "env" },
	{ id: "local", name: "Local Box", auth: "keyless" },
	{ id: "openai", auth: "none" },
	{ id: "zeta", auth: "none" },
	{ id: "mistral", auth: "stored" },
];

/** The pick phase is a real SelectorView; login's pick opens the key
 *  form exactly as the root's continuation does. */
function rigUp(kind: AuthCardKind) {
	const events: Array<{ kind: string; provider?: string; apiKey?: string }> = [];
	let key: AuthKeyCard | undefined;
	const view = new SelectorView(authPickSpec(kind, statuses), {
		onSelect: target => {
			if (kind === "logout") {
				events.push({ kind: "logout", provider: target.id });
			} else {
				key = new AuthKeyCard(target.id, {
					onSubmit: apiKey => events.push({ kind: "login", provider: target.id, apiKey }),
					onClose: () => events.push({ kind: "close" }),
				}, () => {});
			}
		},
		onClose: () => events.push({ kind: "close" }),
	}, () => {});
	return { view, events, key: () => key };
}

describe("the auth provider pick", () => {
	test("login lists exactly the auth:none targets (env/keyless/stored stay out)", () => {
		const { view } = rigUp("login");
		const lines = view.render(60).map(line => line.replace(ANSI, ""));
		assert.ok(lines.some(line => line.includes("openai")));
		assert.ok(lines.some(line => line.includes("zeta")));
		assert.ok(!lines.some(line => line.includes("anthropic")));
		assert.ok(!lines.some(line => line.includes("Local Box")));
		assert.ok(!lines.some(line => line.includes("mistral")));
	});

	test("logout lists exactly the stored rows; the pick IS the command", () => {
		const { view, events } = rigUp("logout");
		const lines = view.render(60).map(line => line.replace(ANSI, ""));
		assert.ok(lines.some(line => line.includes("mistral")));
		assert.ok(!lines.some(line => line.includes("openai")));
		view.handleInput(ENTER);
		assert.deepStrictEqual(events, [{ kind: "logout", provider: "mistral" }]);
	});

	test("escape closes the pick phase", () => {
		const { view, events } = rigUp("login");
		view.handleInput(ESCAPE);
		assert.deepStrictEqual(events, [{ kind: "close" }]);
	});
});

describe("the login key form (the pick's continuation)", () => {
	test("pick advances to the key phase; enter sends provider + key", () => {
		const { view, events, key } = rigUp("login");
		view.handleInput(DOWN); // to zeta
		view.handleInput(UP); // back to openai
		view.handleInput(ENTER);
		assert.deepStrictEqual(events, []); // picked, not yet sent
		assert.ok(key()!.render(60).some(line => line.includes("api key for openai")));
		for (const char of "sk-test-key") key()!.handleInput(char);
		key()!.handleInput(ENTER);
		assert.deepStrictEqual(events, [{ kind: "login", provider: "openai", apiKey: "sk-test-key" }]);
		// The card is closed by the root on send; nothing renders the key.
	});

	test("an empty key stays put (the backend would reject it with the same rule)", () => {
		const { view, events, key } = rigUp("login");
		view.handleInput(ENTER); // pick openai
		key()!.handleInput(ENTER); // empty key — no send
		assert.deepStrictEqual(events, []);
	});

	test("escape closes the key phase", () => {
		const { view, events, key } = rigUp("login");
		view.handleInput(ENTER);
		key()!.handleInput(ESCAPE);
		assert.deepStrictEqual(events, [{ kind: "close" }]);
	});
});
