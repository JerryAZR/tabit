import type { FooterBadgeFactory } from "../registry.ts";
import { formatTokenCount } from "../format.ts";
import { error, warn } from "../../theme.ts";

/** The context meter, the other agents' shape: bar, percent, absolute —
 *  `ctx: █████░░░░░ 28.0% (291k/1M)`. Warn-colored past pi's proven
 *  thresholds (>70 warn, >90 error). Informational only — the backend's
 *  compaction doors own behavior. Gated on the CEILING alone (2026-10):
 *  once `model_selected` states the window the bar renders, with the
 *  not-yet-reported driver as 0 — a fresh session's usage genuinely is
 *  zero, a resume's replay re-sums it within the same beat, and an
 *  always-on bar keeps the footer's shape stable. The spend badges stay
 *  usage-gated: $0.000 at boot would be noise, capacity is not. */
export const createContextBadge: FooterBadgeFactory = () => ({
	id: "context",
	render: facts => {
		if (facts.contextWindow === undefined || facts.contextWindow === 0) {
			return undefined;
		}
		const used = facts.contextUsed ?? 0;
		const pct = (used / facts.contextWindow) * 100;
		const filled = Math.max(0, Math.min(BAR_CELLS, Math.round((pct / 100) * BAR_CELLS)));
		const bar = "█".repeat(filled) + "░".repeat(BAR_CELLS - filled);
		const text = `ctx: ${bar}  ${pct.toFixed(1)}% (${formatTokenCount(used)}/${formatTokenCount(facts.contextWindow)})`;
		if (pct > 90) return error(text);
		if (pct > 70) return warn(text);
		return text;
	},
});

const BAR_CELLS = 10;
