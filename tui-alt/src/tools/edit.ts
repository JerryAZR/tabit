/**
 * `edit`'s card, pi's shape: the call line is `edit {path}`; the result
 * body is the change story — collapsed a one-line `+N added, -M deleted`
 * summary, expanded the unified diff the backend pre-computed in
 * `tool_result.details` (theme-colored `+`/`-`/context lines with hunk
 * headers, unapplied hunks marked from `outcomes`). No diff library: the
 * wire format already is the diff.
 */

import type { ToolCardRenderer, ToolRenderInput } from "./registry";
import { parseToolArgs, registerToolCardRenderer } from "./registry";
import type { EditDetails } from "../protocol";
import { diffAdd, diffContext, diffHunk, diffRemove, toolAccent, toolTitle, toolBold } from "../theme";

registerToolCardRenderer("edit", {
	call: input => {
		const path = parseToolArgs(input)?.path;
		if (typeof path !== "string") return undefined;
		return `${toolTitle(toolBold("edit"))} ${toolAccent(path)}`;
	},

	result(input) {
		const details = parseDetails(input);
		if (details === undefined) return undefined;
		if (!input.expanded) return [changeSummary(details)];
		return diffLines(details);
	},
});

function parseDetails(input: ToolRenderInput): EditDetails | undefined {
	const details = input.details;
	if (typeof details !== "object" || details === null) return undefined;
	const diff = (details as EditDetails).diff;
	if (typeof diff !== "object" || diff === null || !Array.isArray(diff.hunks)) return undefined;
	return details as EditDetails;
}

function changeSummary(details: EditDetails): string {
	let added = 0;
	let removed = 0;
	for (const hunk of details.diff?.hunks ?? []) {
		for (const line of hunk.lines ?? []) {
			if (line.kind === "added") added++;
			if (line.kind === "removed") removed++;
		}
	}
	return `+${added} added, -${removed} deleted`;
}

function diffLines(details: EditDetails): string[] {
	const lines: string[] = [];
	(details.diff?.hunks ?? []).forEach((hunk, index) => {
		lines.push(diffHunk(hunkLabel(details, index)));
		for (const line of hunk.lines ?? []) {
			// ASCII + and - markers: they must align across fonts, and the
			// unicode minus (U+2212) is exactly the ambiguous-width class
			// the marker rule exists for.
			if (line.kind === "added") lines.push(diffAdd(`+ ${line.text}`));
			else if (line.kind === "removed") lines.push(diffRemove(`- ${line.text}`));
			else lines.push(diffContext(`  ${line.text}`));
		}
	});
	return lines;
}

function hunkLabel(details: EditDetails, index: number): string {
	const anchor = details.diff?.hunks?.[index]?.new_start;
	const applied = details.outcomes?.[index]?.applied;
	const state = applied === false ? " · not applied" : "";
	return `@@ +${anchor ?? "?"} @@${state}`;
}
