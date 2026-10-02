import type { FooterBadgeFactory } from "../registry.ts";

/** Liveness: the one state word (owner ruling — the footer owns it; the
 *  status strip renders nothing when idle). Never silent. */
export const createStateBadge: FooterBadgeFactory = () => ({
	id: "state",
	render: facts => (facts.running ? "running" : "idle"),
});
