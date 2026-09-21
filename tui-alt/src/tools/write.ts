/**
 * `write`'s card, pi's shape: the call line is `write {path}` — the path
 * in accent — and the result body previews the written content from the
 * args (what got written is known at call time), ten lines collapsed with
 * a more-lines hint, all lines expanded, themed as output. Errors surface
 * in error color even collapsed.
 */

import { parseToolArgs, registerToolCardRenderer, type ToolRenderInput } from "./registry";
import { toolAccent, toolError, toolMuted, toolOutput, toolTitle, toolBold } from "../theme";

registerToolCardRenderer("write", {
	call: input => {
		const path = parseToolArgs(input)?.path;
		if (typeof path !== "string") return undefined;
		return `${toolTitle(toolBold("write"))} ${toolAccent(path)}`;
	},
	result: input => {
		const source = argsContent(input);
		if (source === undefined) {
			// No content arg to preview: show the model-facing reply instead.
			return input.expanded || !input.ok ? input.content.split("\n").map(line => (input.ok ? toolOutput(line) : toolError(line))) : [];
		}
		const styled = source.split("\n").map(line => toolOutput(line));
		if (input.expanded) return styled;
		const shown = styled.slice(0, 10);
		const remaining = styled.length - shown.length;
		if (remaining > 0) shown.push(toolMuted(`... (${remaining} more lines, ctrl+o to expand)`));
		return shown;
	},
});

function argsContent(input: ToolRenderInput): string | undefined {
	const content = parseToolArgs(input)?.content;
	return typeof content === "string" ? content : undefined;
}
