/**
 * The footer badge vocabulary (owner ruling 2026-09): the footer is a
 * container of badges over the mode's pushed facts — never a joiner of
 * pre-formatted strings. A badge owns its segment's format in its own
 * file and is registered at the one site below; declared order is
 * on-screen order and overflow priority. Absence is semantic: a badge
 * with nothing to say returns `undefined` and the container drops it.
 *
 * Two input homes, by data origin:
 * - wire facts arrive in `FooterFacts` (the mode's pushed snapshot);
 * - local-environment facts (git, network, clock — anything that changes
 *   without a backend event) the badge sources itself: hold a cache,
 *   refresh out of band, call `ctx.requestRender()` when it lands,
 *   because `render` must stay synchronous. `dispose` releases timers
 *   and watchers.
 *
 * Extension-provided badges are a deferred ruling (owner 2026-09): the
 * tabit-ext pipe has no frontend lane, and when one is designed it will
 * carry typed payloads rendered by frontend-side badges — this registry
 * is where those renderers will register, so the frontend-side change
 * stays additive. Nothing here assumes the mode is the only facts source.
 */

import type { FooterFacts } from "../mode.ts";
import { createContextBadge } from "./badges/context.ts";
import { createCostBadge } from "./badges/cost.ts";
import { createModelBadge } from "./badges/model.ts";
import { createStateBadge } from "./badges/state.ts";
import { createStreamBadge } from "./badges/stream.ts";
import { createUsageBadge } from "./badges/usage.ts";

/** What the container hands each badge at construction. Grows only when a
 *  badge genuinely needs a new capability — never as a facts back door. */
export interface FooterBadgeContext {
	requestRender(): void;
}

export interface FooterBadge {
	id: string;
	/** One line segment, or `undefined` when the badge has nothing to say. */
	render(facts: FooterFacts): string | undefined;
	dispose?(): void;
}

export type FooterBadgeFactory = (ctx: FooterBadgeContext) => FooterBadge;

/** The one registration site: import order is display order. */
export const FOOTER_BADGES: FooterBadgeFactory[] = [
	createStreamBadge,
	createModelBadge,
	createContextBadge,
	createCostBadge,
	createUsageBadge,
	createStateBadge,
];
