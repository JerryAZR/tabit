/**
 * The catalog channel's session-long truth (v26). A quiet channel says
 * nothing: fresh/cached/updated/disabled/pending are all healthy or
 * self-resolving — the badge speaks only for `offline`, and names what
 * offline left behind: the cache when one exists (it's all this
 * session gets), nothing when the check failed on a bare machine.
 */

import { warn } from "../../theme.ts";
import type { FooterBadgeFactory } from "../registry.ts";

export const createCatalogBadge: FooterBadgeFactory = () => ({
	id: "catalog",
	render: facts =>
		facts.catalog?.state === "offline"
			? warn(facts.catalog.version !== undefined ? "catalog offline — using cache" : "catalog offline — no cache")
			: undefined,
});
