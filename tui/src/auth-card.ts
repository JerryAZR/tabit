/**
 * The `/login` and `/logout` surfaces (v21/v22) over the
 * `providers_available` status fold — a provider pick, then (login only)
 * the key form. The pick list is the consumer law made literal: login
 * targets `auth: "none"` rows, logout offers `auth: "stored"` rows;
 * `env` is display-only and `keyless` has nothing to remove, so neither
 * appears.
 *
 * The two continuation kinds live here: logout's pick IS the command
 * (direct processing), login's pick opens the key form (a custom UI).
 * The pick itself is the shared SelectorView (`authPickSpec`); the key
 * form is the small dedicated component below.
 *
 * The key is unmasked (owner ruling — pi-tui's Input has no password
 * mode and shoulder-surfing is not the threat model) but never echoed
 * post-submit and never enters the editor history: the card owns its
 * own `Input`, not the history-keeping editor. Sending is
 * fire-and-forget like every command — the re-announced
 * `models_available` + `providers_available` pair is the ack, an
 * `notice { id: "auth.failed" }` the failure notice. Enter on an empty
 * key stays put — the backend would reject it with the same rule; no
 * reason to spend the roundtrip.
 */

import { Input, type Component } from "@earendil-works/pi-tui";

import type { SelectorSpec } from "./components/selector.ts";
import { accent, dim } from "./theme.ts";
import type { ProviderStatus } from "./protocol.ts";

/** The card's kind selects its target rows and whether a key phase
 *  follows the pick. */
export type AuthCardKind = "login" | "logout";

/** The provider pick's SelectorView spec: the target rows, the kind's
 *  title, the 10-row viewport. No filter — a small target list. */
export function authPickSpec(kind: AuthCardKind, statuses: readonly ProviderStatus[]): SelectorSpec<ProviderStatus, void> {
	return {
		entries: statuses.filter(s => s.auth === (kind === "login" ? "none" : "stored")),
		title: kind === "login" ? " log in — providers missing keys" : " log out — stored keys",
		pageSize: 10,
		row: target => `${target.name ?? target.id} ${dim(`(${target.id})`)}`,
	};
}

export interface AuthKeyHooks {
	/** A non-empty key, trimmed. */
	onSubmit(apiKey: string): void;
	onClose(): void;
}

/** The login key phase: one Input and its explainer. Escape closes;
 *  enter on an empty key stays put (the backend rejects it with the
 *  same rule — no reason to spend the roundtrip). */
export class AuthKeyCard implements Component {
	readonly focusable = true;
	readonly #provider: string;
	readonly #input = new Input({ placeholder: "paste or type the key…" });
	readonly #hooks: AuthKeyHooks;
	readonly #requestRender: () => void;

	constructor(provider: string, hooks: AuthKeyHooks, requestRender: () => void) {
		this.#provider = provider;
		this.#hooks = hooks;
		this.#requestRender = requestRender;
		this.#input.onSubmit = () => {
			const key = this.#input.getValue().trim();
			if (key !== "") this.#hooks.onSubmit(key);
		};
		this.#input.onEscape = () => this.#hooks.onClose();
	}

	handleInput(data: string): void {
		this.#input.handleInput(data);
		this.#requestRender();
	}

	render(width: number): string[] {
		const rule = dim("─".repeat(Math.max(1, width)));
		return [
			rule,
			` ${accent(`api key for ${this.#provider}`)} — stored in auth.toml, never echoed`,
			...this.#input.render(width),
			dim(" enter stores · esc closes"),
			rule,
		];
	}

	invalidate(): void {}
}
