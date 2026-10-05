/**
 * The focused-stream badge: the footer's "watching" label (codex's
 * pattern, M2-DESIGN.md) — present only when a non-root stream is
 * focused; absence means the root session.
 */

import { accent } from "../../theme.ts";
import type { FooterBadge } from "../registry.ts";

export function createStreamBadge(): FooterBadge {
	return {
		id: "stream",
		render: facts => (facts.streamLabel === undefined ? undefined : accent(`⏎ ${facts.streamLabel}`)),
	};
}
