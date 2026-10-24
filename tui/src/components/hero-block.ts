/**
 * The boot hero: a fresh session's landing page — identity, the
 * keybinding hint, and the boot facts as bracketed sections (pi's
 * startup shape). A landing page, not a truth surface (owner ruling
 * 2026-10): it re-renders when the mode re-folds a fact (a `/login`'s
 * catalog re-announcement included) with no freeze machinery — the
 * footer carries the live truth. Resumed sessions get the plain notes
 * instead (the replay dominates).
 *
 * Sections with no data never render a line (a zero-config boot shows
 * identity + hints + session + cwd). The identity line's version is the
 * view's own build fact — the seam carries wire facts only.
 */

import { accent, dim } from "../theme.ts";
import type { HeroFacts } from "../mode.ts";
import { compactTokens } from "../model-picker.ts";

export class HeroBlock {
	readonly #version: string;
	readonly #touch: () => void;
	#facts: HeroFacts | undefined;

	constructor(version: string, touch: () => void) {
		this.#version = version;
		this.#touch = touch;
	}

	setFacts(facts: HeroFacts): void {
		this.#facts = facts;
		this.#touch();
	}

	render(width: number): string[] {
		void width;
		const lines: string[] = [
			"",
			` ${accent("tabit")} ${dim(this.#version)}`,
			dim(" esc interrupt · / commands · ctrl+o expand · /help for keys"),
			"",
		];
		const facts = this.#facts;
		if (facts === undefined) return lines;
		const sections: Array<[string, string]> = [];
		sections.push([
			"session",
			facts.sessionsOnDisk === undefined
				? facts.sessionId
				: `${facts.sessionId} — ${facts.sessionsOnDisk} session(s) on disk`,
		]);
		if (facts.model !== undefined) {
			const m = facts.model;
			const parts = [m.name, `(${m.provider})`];
			if (m.thinkingLevel !== undefined && m.thinkingLevel !== null) parts.push(`· ${m.thinkingLevel}`);
			if (m.contextWindow !== undefined) parts.push(`· ${compactTokens(m.contextWindow)} ctx`);
			sections.push(["model", parts.join(" ")]);
		}
		if (facts.skills !== undefined && facts.skills.length > 0) {
			sections.push(["skills", facts.skills.join(", ")]);
		}
		if (facts.models !== undefined && facts.models.models > 0) {
			sections.push(["models", `${facts.models.models} across ${facts.models.providers} provider(s)`]);
		}
		if (facts.extensions !== undefined && facts.extensions.names.length > 0) {
			const dead = facts.extensions.dead > 0 ? ` — ${facts.extensions.dead} dead` : "";
			sections.push(["extensions", facts.extensions.names.join(", ") + dead]);
		}
		if (facts.cwd !== undefined) sections.push(["cwd", facts.cwd]);
		const labelWidth = Math.max(...sections.map(([label]) => label.length + 2));
		for (const [label, text] of sections) {
			lines.push(` ${accent(`[${label}]`.padEnd(labelWidth))}  ${dim(text)}`);
		}
		lines.push("");
		return lines;
	}

	invalidate(): void {}
}
