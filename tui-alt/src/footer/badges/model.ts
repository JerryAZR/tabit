import type { FooterBadgeFactory } from "../registry.ts";

/** The active model: config's display name when stated, else the id;
 *  a named thinking level rides as a suffix (`GLM-5.3 (high)`) — the
 *  default (null) and the unstated stay bare. */
export const createModelBadge: FooterBadgeFactory = () => ({
	id: "model",
	render: facts => {
		const name = facts.modelName ?? facts.model;
		return name !== undefined && typeof facts.thinkingLevel === "string" ? `${name} (${facts.thinkingLevel})` : name;
	},
});
