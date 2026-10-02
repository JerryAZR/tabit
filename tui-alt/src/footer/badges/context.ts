import type { FooterBadgeFactory } from "../registry.ts";
import { formatTokenCount } from "../format.ts";
import { error, warn } from "../../theme.ts";

/** The context meter, the other agents' shape: bar, percent, absolute —
 *  `ctx: █████░░░░░ 28.0% (291k/1M)`. Warn-colored past pi's proven
 *  thresholds (>70 warn, >90 error). Informational only — the backend's
 *  compaction doors own behavior. Silent until both facts exist: the
 *  first report (`completion_call` total or `compaction_end`'s
 *  `tokens_after`) and a stated context window. */
export const createContextBadge: FooterBadgeFactory = () => ({
	id: "context",
	render: facts => {
		if (facts.contextUsed === undefined || facts.contextWindow === undefined || facts.contextWindow === 0) {
			return undefined;
		}
		const pct = (facts.contextUsed / facts.contextWindow) * 100;
		const filled = Math.max(0, Math.min(BAR_CELLS, Math.round((pct / 100) * BAR_CELLS)));
		const bar = "█".repeat(filled) + "░".repeat(BAR_CELLS - filled);
		const text = `ctx: ${bar}  ${pct.toFixed(1)}% (${formatTokenCount(facts.contextUsed)}/${formatTokenCount(facts.contextWindow)})`;
		if (pct > 90) return error(text);
		if (pct > 70) return warn(text);
		return text;
	},
});

const BAR_CELLS = 10;
