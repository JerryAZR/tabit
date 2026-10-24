/**
 * The boot hero: the mode emits facts on a fresh root (and suppresses
 * the summary notes it replaces), never on a resume; the block renders
 * identity + sections, hiding absent data.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { HeroBlock } from "../src/components/hero-block.ts";
import type { HeroFacts } from "../src/mode.ts";

const ANSI = /\x1b\[[0-9;]*m/g;
const plain = (s: string) => s.replace(ANSI, "");

describe("HeroBlock", () => {
	test("identity + hints always; sections only when their facts exist", () => {
		const hero = new HeroBlock("0.1.0", () => {});
		const bare = plain(hero.render(100).join("\n"));
		assert.ok(bare.includes("tabit 0.1.0"));
		assert.ok(bare.includes("/help for keys"));
		assert.ok(!bare.includes("[model]"));
		assert.ok(!bare.includes("[skills]"));

		hero.setFacts({
			sessionId: "s-1",
			cwd: "C:\\w",
			sessionsOnDisk: 2,
			model: { name: "GLM-5.3", provider: "glm", contextWindow: 1_000_000, thinkingLevel: "high" },
			skills: ["commit", "review"],
			models: { models: 4, providers: 2 },
			extensions: { names: ["echo-ext"], dead: 1 },
		});
		const full = plain(hero.render(100).join("\n"));
		assert.ok(full.includes("[session]"));
		assert.ok(full.includes("s-1 — 2 session(s) on disk"));
		assert.ok(full.includes("[model]"));
		assert.ok(full.includes("GLM-5.3 (glm) · high · 1M ctx"));
		assert.ok(full.includes("[skills]"));
		assert.ok(full.includes("commit, review"));
		assert.ok(full.includes("[models]"));
		assert.ok(full.includes("4 across 2 provider(s)"));
		assert.ok(full.includes("[extensions]"));
		assert.ok(full.includes("echo-ext — 1 dead"));
		assert.ok(full.includes("[cwd]"));
		assert.ok(full.includes("C:\\w"));
	});

	test("unstated facts render nothing, not zero", () => {
		const hero = new HeroBlock("0.1.0", () => {});
		hero.setFacts({
			sessionId: "s-1",
			cwd: undefined,
			sessionsOnDisk: undefined,
			model: undefined,
			skills: undefined,
			models: undefined,
			extensions: undefined,
		});
		const rendered = plain(hero.render(100).join("\n"));
		assert.ok(rendered.includes("s-1")); // the id alone — the one always-known fact
		assert.ok(!rendered.includes("[model]"));
		assert.ok(!rendered.includes("[cwd]"));
		assert.ok(!rendered.includes("session(s) on disk"));
	});
});
