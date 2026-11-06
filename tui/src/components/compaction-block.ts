/**
 * The compaction row: a dedicated transcript surface for `compaction_end`,
 * never an info NoteBlock — compaction is a first-class lifecycle event
 * with its own data, and the row carries it: the pre-compaction context
 * length (captured at `compaction_begin`, before the summarizer's own
 * steps can pollute the meter) against the authoritative `tokens_after`.
 * Purple accent on a single quiet line — clearly neither conversation,
 * nor a notice callout, nor the gray info chatter. Leads with the
 * transcript's uniform blank line.
 */

import { Text, type Component } from "@earendil-works/pi-tui";

import { formatTokenCount } from "../footer/format.ts";
import { marker } from "../symbols.ts";
import { compaction } from "../theme.ts";

export class CompactionBlock implements Component {
	readonly #line: Text;

	constructor(before: number | undefined, after: number) {
		const detail = before === undefined
			? `context now ${formatTokenCount(after)} tokens`
			: `${formatTokenCount(before)} → ${formatTokenCount(after)} tokens`;
		this.#line = new Text(compaction(marker("↧", `context compacted — ${detail}`)), 0, 0);
	}

	render(width: number): string[] {
		return ["", ...this.#line.render(width)];
	}

	invalidate(): void {
		this.#line.invalidate();
	}
}
