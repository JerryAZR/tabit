/**
 * The `/login` + `/logout` card, headless: the consumer law made literal
 * (login lists `auth: "none"`, logout lists `auth: "stored"`), the
 * two-phase login flow (pick → key), and the copy discipline — the key
 * reaches the hook and is never echoed in the render after submit.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { AuthCardView, type AuthCardHooks, type AuthCardKind } from "../src/auth-card.ts";
import { applyKeybindings } from "../src/keybindings.ts";
import type { ProviderStatus } from "../src/protocol.ts";

// The card resolves select actions through the global registry — installed
// in production by root.bind. Install the defaults here.
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

function rigUp(kind: AuthCardKind) {
	const events: Array<{ kind: string; provider?: string; apiKey?: string }> = [];
	const hooks: AuthCardHooks = {
		onLogin: (provider, apiKey) => events.push({ kind: "login", provider, apiKey }),
		onLogout: provider => events.push({ kind: "logout", provider }),
		onClose: () => events.push({ kind: "close" }),
	};
	const view = new AuthCardView(kind, statuses, hooks, () => {});
	return { view, events };
}

describe("AuthCardView", () => {
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

	test("login: pick advances to the key phase; enter sends provider + key", () => {
		const { view, events } = rigUp("login");
		view.handleInput(DOWN); // to zeta
		view.handleInput(UP); // back to openai
		view.handleInput(ENTER);
		assert.deepStrictEqual(events, []); // picked, not yet sent
		assert.ok(view.render(60).some(line => line.includes("api key for openai")));
		for (const char of "sk-test-key") view.handleInput(char);
		view.handleInput(ENTER);
		assert.deepStrictEqual(events, [{ kind: "login", provider: "openai", apiKey: "sk-test-key" }]);
		// The card is closed by the root on send; nothing renders the key.
	});

	test("an empty key stays put (the backend would reject it with the same rule)", () => {
		const { view, events } = rigUp("login");
		view.handleInput(ENTER); // pick openai
		view.handleInput(ENTER); // empty key — no send
		assert.deepStrictEqual(events, []);
	});

	test("escape closes from either phase", () => {
		const pickPhase = rigUp("login");
		pickPhase.view.handleInput(ESCAPE);
		assert.deepStrictEqual(pickPhase.events, [{ kind: "close" }]);

		const keyPhase = rigUp("login");
		keyPhase.view.handleInput(ENTER);
		keyPhase.view.handleInput(ESCAPE);
		assert.deepStrictEqual(keyPhase.events, [{ kind: "close" }]);
	});
});
