/**
 * The footer badges against plain fact records: pure segments, absence as
 * `undefined`, registry order as display order. No TUI boots here — the
 * container is a join, and the join is testable as a string.
 */

import { describe, expect, test } from "bun:test";

import { FooterBar } from "../src/footer/footer-bar";
import { FOOTER_BADGES, type FooterBadgeFactory } from "../src/footer/registry";
import type { FooterFacts } from "../src/mode";

const facts = (over: Partial<FooterFacts> = {}): FooterFacts => ({
	session: "s1",
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
		expect(model({ requestRender: () => {} }).id).toBe("model");
		expect(renderOne(model, { modelName: "Kimi K2.8 Preview" })).toBe("Kimi K2.8 Preview");
		expect(renderOne(model)).toBe("kimi-for-coding");
		expect(renderOne(model, { model: undefined })).toBeUndefined();
	});

	test("context: bar, percent, absolute; silent until both facts exist; thresholds colorize", () => {
		const context = FOOTER_BADGES[1]!;
		expect(renderOne(context)).toBeUndefined();
		expect(renderOne(context, { contextUsed: 1000 })).toBeUndefined();
		expect(renderOne(context, { contextWindow: 0, contextUsed: 1000 })).toBeUndefined();
		const full = renderOne(context, { contextWindow: 1_000_000, contextUsed: 291_000 })!;
		expect(full).toMatch(/^ctx: /);
		expect(full).toContain("█".repeat(3) + "░".repeat(7));
		expect(full).toContain("29.1% (291k/1.0M)");
		expect(renderOne(context, { contextWindow: 200_000, contextUsed: 150_000 })).toMatch(/\x1b\[33m/);
		expect(renderOne(context, { contextWindow: 200_000, contextUsed: 190_000 })).toMatch(/\x1b\[31m/);
	});

	test("cost: recorded dollars as recorded; silent until the first costed turn", () => {
		const cost = FOOTER_BADGES[2]!;
		expect(renderOne(cost)).toBeUndefined();
		expect(renderOne(cost, { cost: 0.000152 })).toBe("$0.000152"); // sub-cent stays honest
		expect(renderOne(cost, { cost: 4.271 })).toBe("$4.27");
	});

	test("usage: labeled token breakdown, cache writes ignored, hit rate with the cached leg", () => {
		const usage = FOOTER_BADGES[3]!;
		expect(usage({ requestRender: () => {} }).id).toBe("usage");
		expect(renderOne(usage)).toBeUndefined();
		expect(renderOne(usage, { inputTokens: 421, outputTokens: 137 })).toBe("in 421  out 137");
		expect(
			renderOne(usage, {
				inputTokens: 5_300,
				outputTokens: 2_400_000,
				cachedInputTokens: 8_100,
				cacheCreationTokens: 999_999, // carried as data, never rendered
				cacheHitRate: 96.27,
			}),
		).toBe("in 5.3k  out 2.4M  cached 8.1k (96.3%)");
		// The rate rides only with cached volume; writes alone surface nothing.
		expect(renderOne(usage, { inputTokens: 421, outputTokens: 137, cacheHitRate: 50 })).toBe("in 421  out 137");
		// All-cached edge: in/out show their true zero.
		expect(renderOne(usage, { cachedInputTokens: 8_100 })).toBe("in 0  out 0  cached 8.1k");
	});

	test("state: the one state word, never silent", () => {
		const state = FOOTER_BADGES[4]!;
		expect(renderOne(state, { running: true })).toBe("running");
		expect(renderOne(state, { running: false })).toBe("idle");
	});
});

describe("footer registry and container", () => {
	test("the registry is the display order; ids are unique", () => {
		expect(FOOTER_BADGES.map(f => f({ requestRender: () => {} }).id)).toEqual([
			"model",
			"context",
			"cost",
			"usage",
			"state",
		]);
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
		expect(line).toContain("Kimi K2.8 Preview");
		expect(line.indexOf("Kimi K2.8 Preview")).toBeLessThan(line.indexOf("ctx:"));
		expect(line.indexOf("ctx:")).toBeLessThan(line.indexOf("$4.27"));
		expect(line.indexOf("$4.27")).toBeLessThan(line.indexOf("in 421"));
		expect(line.indexOf("in 421")).toBeLessThan(line.indexOf("running"));
		expect(rendered.length).toBeGreaterThan(0);
		footer.dispose();
	});

	test("before any content the line is just the state word", () => {
		const footer = new FooterBar(() => {});
		footer.set(facts());
		const line = footer.render(200).join("");
		expect(line).toContain("idle");
		expect(line).not.toContain("in ");
		expect(line).not.toContain("$");
		expect(line).not.toContain("ctx:");
	});
});
