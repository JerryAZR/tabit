import type { FooterBadgeFactory } from "../registry";
import { formatTokenCount } from "../format";

/**
 * The usage badge: the session's token breakdown, labeled — token usage
 * is independent of price (the total cost badge carries the money side),
 * so its shape follows usage, not the billing legs. Cache writes are
 * ignored (owner ruling): `in` is uncached input, `cached` is what the
 * provider served from cache, and the parenthetical is the latest
 * request's hit rate. Silent before the first request.
 */
export const createUsageBadge: FooterBadgeFactory = () => ({
	id: "usage",
	render: facts => {
		const { inputTokens, outputTokens, cachedInputTokens, cacheHitRate } = facts;
		if (inputTokens === 0 && outputTokens === 0 && cachedInputTokens === 0) return undefined;
		const parts = [`in ${formatTokenCount(inputTokens)}`, `out ${formatTokenCount(outputTokens)}`];
		if (cachedInputTokens > 0) {
			const rate = cacheHitRate !== undefined ? ` (${cacheHitRate.toFixed(1)}%)` : "";
			parts.push(`cached ${formatTokenCount(cachedInputTokens)}${rate}`);
		}
		return parts.join("  ");
	},
});
