/**
 * The focused-stream badge: the footer's "watching" label (codex's
 * pattern, M2-DESIGN.md) — present only when a non-root stream is
 * focused; absence means the root session.
 */

import { accent, dim } from "../../theme.ts";
import type { FooterBadge } from "../registry.ts";

export function createStreamBadge(): FooterBadge {
	return {
		id: "stream",
		// A closed focused stream keeps its label, dimmed — viewing only.
		render: facts =>
			facts.streamLabel === undefined
				? undefined
				: facts.closed
					? dim(`⏎ ${facts.streamLabel} (closed)`)
					: accent(`⏎ ${facts.streamLabel}`),
	};
}
