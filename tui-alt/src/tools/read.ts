/**
 * `read`'s card, pi's shape: the call line IS the collapsed view —
 * `read {path}`, the path in accent, with the `:start-end` range in warn
 * when the call paged — and the result body stays empty collapsed (the
 * file's content is the model's business, not the transcript's) and
 * shows themed content expanded. Errors surface even collapsed.
 */

import { parseToolArgs, registerToolCardRenderer, type ToolRenderInput } from "./registry";
import { toolAccent, toolError, toolMuted, toolOutput, toolTitle, toolBold, warn } from "../theme";

registerToolCardRenderer("read", {
	call: input => {
		const path = parseToolArgs(input)?.path;
		if (typeof path !== "string") return undefined;
		return `${toolTitle(toolBold("read"))} ${toolAccent(path)}${lineRange(input)}`;
	},
	result: input => {
		if (!input.expanded && input.ok) return [];
		const styled = input.content.split("\n").map(line => (input.ok ? toolOutput(line) : toolError(line)));
		if (!input.expanded) {
			const shown = styled.slice(0, 10);
			const remaining = styled.length - shown.length;
			if (remaining > 0) shown.push(toolMuted(`... (${remaining} more lines, ctrl+o to expand)`));
			return shown;
		}
		return styled;
	},
});

function lineRange(input: ToolRenderInput): string {
	const args = parseToolArgs(input);
	const offset = typeof args?.offset === "number" ? args.offset : undefined;
	const limit = typeof args?.limit === "number" ? args.limit : undefined;
	if (offset === undefined && limit === undefined) return "";
	const start = offset ?? 1;
	const end = limit !== undefined ? start + limit - 1 : "";
	return warn(`:${start}${end ? `-${end}` : ""}`);
}
