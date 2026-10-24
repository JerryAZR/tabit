/**
 * The `/login` and `/logout` card (v21/v22): one auth surface over the
 * `providers_available` status fold — pick a provider from the relevant
 * targets, then (login only) collect the key. The pick list is the
 * consumer law made literal: login targets `auth: "none"` rows, logout
 * offers `auth: "stored"` rows; `env` is display-only and `keyless` has
 * nothing to remove, so neither appears.
 *
 * The key is unmasked (owner ruling — pi-tui's Input has no password
 * mode and shoulder-surfing is not the threat model) but never echoed
 * post-submit and never enters the editor history: the card owns its
 * own `Input`, not the history-keeping editor. Sending is
 * fire-and-forget like every command — the re-announced
 * `models_available` + `providers_available` pair is the ack, an
 * `error { kind: "auth" }` the failure note.
 *
 * Keys: ↑/↓ move (wrapping), enter advances/confirms, escape closes.
 * The key phase's enter on an empty key stays put — the backend would
 * reject it with the same rule; no reason to spend the roundtrip.
 */

import { getKeybindings, Input, truncateToWidth, type Component } from "@earendil-works/pi-tui";

import { accent, dim } from "./theme.ts";
import type { ProviderStatus } from "./protocol.ts";

export interface AuthCardHooks {
	onLogin(provider: string, apiKey: string): void;
	onLogout(provider: string): void;
	onClose(): void;
}

/** The card's kind selects its target rows and whether a key phase
 *  follows the pick. */
export type AuthCardKind = "login" | "logout";

const MAX_VISIBLE_ROWS = 10;

export class AuthCardView implements Component {
	readonly focusable = true;
	readonly #kind: AuthCardKind;
	/** The target rows: login → `auth: "none"`, logout → `auth: "stored"`. */
	readonly #targets: ProviderStatus[];
	#cursor = 0;
	/** Set once a provider is picked (login only) — the key phase. */
	#picked: string | undefined;
	readonly #input = new Input({ placeholder: "paste or type the key…" });
	readonly #hooks: AuthCardHooks;
	readonly #requestRender: () => void;

	constructor(kind: AuthCardKind, statuses: readonly ProviderStatus[], hooks: AuthCardHooks, requestRender: () => void) {
		this.#kind = kind;
		this.#targets = statuses.filter(s => s.auth === (kind === "login" ? "none" : "stored"));
		this.#hooks = hooks;
		this.#requestRender = requestRender;
		this.#input.onSubmit = () => this.#submitKey();
		this.#input.onEscape = () => this.#hooks.onClose();
	}

	handleInput(data: string): void {
		const kb = getKeybindings();
		if (this.#picked !== undefined) {
			// The key phase: text editing delegates to the Input (its
			// onSubmit/onEscape come back as confirm/close).
			this.#input.handleInput(data);
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.select.up")) {
			this.#cursor = this.#cursor === 0 ? this.#targets.length - 1 : this.#cursor - 1;
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.select.down")) {
			this.#cursor = this.#cursor === this.#targets.length - 1 ? 0 : this.#cursor + 1;
			this.#requestRender();
			return;
		}
		if (kb.matches(data, "tui.select.cancel")) {
			this.#hooks.onClose();
			return;
		}
		if (kb.matches(data, "tui.select.confirm")) {
			const target = this.#targets[this.#cursor];
			if (target === undefined) return;
			if (this.#kind === "logout") {
				// One phase: the pick IS the command (total and idempotent).
				this.#hooks.onLogout(target.id);
				return;
			}
			this.#picked = target.id;
			this.#requestRender();
		}
	}

	#submitKey(): void {
		const key = this.#input.getValue().trim();
		if (key === "") return; // the backend rejects empty keys with the same rule
		this.#hooks.onLogin(this.#picked!, key);
	}

	invalidate(): void {}

	render(width: number): string[] {
		const rule = dim("─".repeat(Math.max(1, width)));
		if (this.#picked !== undefined) {
			return [
				rule,
				` ${accent(`api key for ${this.#picked}`)} — stored in auth.toml, never echoed`,
				...this.#input.render(width),
				dim(" enter stores · esc closes"),
				rule,
			];
		}
		const lines: string[] = [rule, dim(this.#kind === "login" ? " log in — providers missing keys" : " log out — stored keys")];
		this.#cursor = Math.min(this.#cursor, this.#targets.length - 1);
		const startIndex = Math.max(0, Math.min(this.#cursor - Math.floor(MAX_VISIBLE_ROWS / 2), this.#targets.length - MAX_VISIBLE_ROWS));
		const endIndex = Math.min(startIndex + MAX_VISIBLE_ROWS, this.#targets.length);
		for (let index = startIndex; index < endIndex; index++) {
			const target = this.#targets[index]!;
			const cursor = index === this.#cursor ? accent("❯") : " ";
			lines.push(truncateToWidth(` ${cursor} ${target.name ?? target.id} ${dim(`(${target.id})`)}`, width));
		}
		lines.push(dim(" enter selects · esc closes"));
		lines.push(rule);
		return lines;
	}
}
