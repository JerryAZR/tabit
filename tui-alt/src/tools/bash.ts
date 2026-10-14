/**
 * `bash`'s card, pi's shape: the call line is the command itself —
 * `bash {command}`, with `(timeout Ns)` in muted when the call set one —
 * and the result body is themed output: the **tail** five lines collapsed
 * (with an earlier-lines hint above them — what happened last is what
 * matters), everything expanded, and the muted `Took` line at the end
 * (absent on replay, where durations are unrecoverable).
 */

import { parseToolArgs, registerToolCardRenderer, type ToolRenderInput } from "./registry.ts";
import { toolBold, toolMuted, toolOutput, toolTitle } from "../theme.ts";

registerToolCardRenderer("bash", {
	call: input => {
		const command = parseToolArgs(input)?.command;
		if (typeof command !== "string") return undefined;
		const suffix = timeoutSuffix(input);
		return toolTitle(toolBold(`bash ${command.replace(/\s+/g, " ").trim()}`)) + suffix;
	},
	result: input => {
		const lines = input.content.split("\n").map(line => toolOutput(line));
		const body: string[] = [];
		if (!input.expanded && lines.length > PREVIEW_LINES) {
			body.push(toolMuted(`... (${lines.length - PREVIEW_LINES} earlier lines, ctrl+o to expand)`));
			body.push(...lines.slice(-PREVIEW_LINES));
		} else {
			body.push(...lines);
		}
		if (input.elapsedMs !== undefined) body.push("", toolMuted(`Took ${(input.elapsedMs / 1000).toFixed(1)}s`));
		return body;
	},
});

const PREVIEW_LINES = 5;

function timeoutSuffix(input: ToolRenderInput): string {
	const timeout = parseToolArgs(input)?.timeout_secs;
	return typeof timeout === "number" ? toolMuted(` (timeout ${timeout}s)`) : "";
}
