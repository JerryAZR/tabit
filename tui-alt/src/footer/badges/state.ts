import type { FooterBadgeFactory } from "../registry.ts";

/** Liveness: the one state word (owner ruling — the footer owns it; the
 *  status strip renders nothing when idle). Never silent. A closed
 *  stream's word is "closed" — it is not "idle" (idle implies a run
 *  could start; a closed stream never runs again). */
export const createStateBadge: FooterBadgeFactory = () => ({
	id: "state",
	render: facts => (facts.closed ? "closed" : facts.running ? "running" : "idle"),
});
