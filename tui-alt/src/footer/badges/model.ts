import type { FooterBadgeFactory } from "../registry.ts";

/** The active model: config's display name when stated, else the id. */
export const createModelBadge: FooterBadgeFactory = () => ({
	id: "model",
	render: facts => facts.modelName ?? facts.model,
});
