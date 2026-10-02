/**
 * The footer badges against plain fact records: pure segments, absence as
 * `undefined`, registry order as display order. No TUI boots here — the
 * container is a join, and the join is testable as a string.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { FooterBar } from "../src/footer/footer-bar.ts";
import { FOOTER_BADGES, type FooterBadgeFactory } from "../src/footer/registry.ts";
import type { FooterFacts } from "../src/mode.ts";

const facts = (over: Partial<FooterFacts> = {}): FooterFacts => ({
	session: "s1",
	path: undefined,
	cwd: undefined,
	model: "kimi-for-coding",
	modelName: undefined,
	contextWindow: undefined,
	resumed: false,
	inputTokens: 0,
	outputTokens: 0,
	cachedInputTokens: 0,
	cacheCreationTokens: 0,
	cacheHitRate: undefined,
	cost: undefined,
	contextUsed: undefined,
	rates: undefined,
	running: false,
	...over,
});

function renderOne(factory: FooterBadgeFactory, over: Partial<FooterFacts> = {}): string | undefined {
	return factory({ requestRender: () => {} }).render(facts(over));
}

describe("footer badges", () => {
	test("model: display name wins, id is the fallback, silence when neither", () => {
		const model = FOOTER_BADGES[0]!;
		assert.strictEqual(model({ requestRender: () => {} }).id, "model");
		assert.strictEqual(renderOne(model, { modelName: "Kimi K2.8 Preview" }), "Kimi K2.8 Preview");
		assert.strictEqual(renderOne(model), "kimi-for-coding");
		assert.strictEqual(renderOne(model, { model: undefined }), undefined);
	});

	test("context: bar, percent, absolute; silent until both facts exist; thresholds colorize", () => {
		const context = FOOTER_BADGES[1]!;
		assert.strictEqual(renderOne(context), undefined);
		assert.strictEqual(renderOne(context, { contextUsed: 1000 }), undefined);
		assert.strictEqual(renderOne(context, { contextWindow: 0, contextUsed: 1000 }), undefined);
		const full = renderOne(context, { contextWindow: 1_000_000, contextUsed: 291_000 })!;
		assert.match(full, /^ctx: /);
		assert.ok(full.includes("█".repeat(3) + "░".repeat(7)));
		assert.ok(full.includes("29.1% (291k/1.0M)"));
		assert.match(renderOne(context, { contextWindow: 200_000, contextUsed: 150_000 })!, /\x1b\[33m/);
		assert.match(renderOne(context, { contextWindow: 200_000, contextUsed: 190_000 })!, /\x1b\[31m/);
	});

	test("cost: recorded dollars as recorded; silent until the first costed turn", () => {
		const cost = FOOTER_BADGES[2]!;
		assert.strictEqual(renderOne(cost), undefined);
		assert.strictEqual(renderOne(cost, { cost: 0.000152 }), "$0.000152"); // sub-cent stays honest
		assert.strictEqual(renderOne(cost, { cost: 4.271 }), "$4.27");
	});

	test("usage: labeled token breakdown, cache writes ignored, hit rate with the cached leg", () => {
		const usage = FOOTER_BADGES[3]!;
		assert.strictEqual(usage({ requestRender: () => {} }).id, "usage");
		assert.strictEqual(renderOne(usage), undefined);
		assert.strictEqual(renderOne(usage, { inputTokens: 421, outputTokens: 137 }), "in 421  out 137");
		assert.strictEqual(
			renderOne(usage, {
				inputTokens: 5_300,
				outputTokens: 2_400_000,
				cachedInputTokens: 8_100,
				cacheCreationTokens: 999_999, // carried as data, never rendered
				cacheHitRate: 96.27,
			}),
			"in 5.3k  out 2.4M  cached 8.1k (96.3%)",
		);
		// The rate rides only with cached volume; writes alone surface nothing.
		assert.strictEqual(renderOne(usage, { inputTokens: 421, outputTokens: 137, cacheHitRate: 50 }), "in 421  out 137");
		// All-cached edge: in/out show their true zero.
		assert.strictEqual(renderOne(usage, { cachedInputTokens: 8_100 }), "in 0  out 0  cached 8.1k");
	});

	test("state: the one state word, never silent", () => {
		const state = FOOTER_BADGES[4]!;
		assert.strictEqual(renderOne(state, { running: true }), "running");
		assert.strictEqual(renderOne(state, { running: false }), "idle");
	});
});

describe("footer registry and container", () => {
	test("the registry is the display order; ids are unique", () => {
		assert.deepStrictEqual(
			FOOTER_BADGES.map(f => f({ requestRender: () => {} }).id),
			["model", "context", "cost", "usage", "state"],
		);
	});

	test("the container joins defined segments in order and drops the silent ones", () => {
		const rendered: string[] = [];
		const footer = new FooterBar(() => rendered.push("r"));
		footer.set(
			facts({
				modelName: "Kimi K2.8 Preview",
				contextWindow: 200_000,
				contextUsed: 6_000,
				cost: 4.271,
				inputTokens: 421,
				outputTokens: 137,
				running: true,
			}),
		);
		const line = footer.render(200).join("");
		assert.ok(line.includes("Kimi K2.8 Preview"));
		assert.ok(line.indexOf("Kimi K2.8 Preview") < line.indexOf("ctx:"));
		assert.ok(line.indexOf("ctx:") < line.indexOf("$4.27"));
		assert.ok(line.indexOf("$4.27") < line.indexOf("in 421"));
		assert.ok(line.indexOf("in 421") < line.indexOf("running"));
		assert.ok(rendered.length > 0);
		footer.dispose();
	});

	test("before any content the line is just the state word", () => {
		const footer = new FooterBar(() => {});
		footer.set(facts());
		const line = footer.render(200).join("");
		assert.ok(line.includes("idle"));
		assert.ok(!line.includes("in "));
		assert.ok(!line.includes("$"));
		assert.ok(!line.includes("ctx:"));
	});
});
