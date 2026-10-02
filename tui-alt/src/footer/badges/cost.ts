import type { FooterBadgeFactory } from "../registry.ts";

/** Session spend: the v13 recorded dollars, displayed as recorded — never
 *  re-derived from rates or tokens. Three significant digits, not fixed
 *  decimals: early sessions sit far below a cent, and fixed 3-decimal
 *  formatting would show the forbidden $0.000. Silent until the first
 *  costed turn. */
export const createCostBadge: FooterBadgeFactory = () => ({
	id: "cost",
	render: facts => (facts.cost === undefined ? undefined : `$${facts.cost.toPrecision(3)}`),
});
