/**
 * `subagent`'s card: the call line is the delegated task itself —
 * `subagent {task}` (whitespace-collapsed like bash's command), with a
 * muted `(cwd: …)` suffix when the call scoped the child elsewhere. The
 * result body is the child's final answer through the default
 * preview/expand law. This card is the ONE rendering of a delegation:
 * the transcript note that used to announce "subagent started: …" only
 * existed because the default card showed a bare name — it died with
 * this registration (2026-10), and the child's progress/focus lives in
 * the subagent list.
 */

import { parseToolArgs, registerToolCardRenderer, type ToolRenderInput } from "./registry.ts";
import { toolBold, toolMuted, toolTitle } from "../theme.ts";

registerToolCardRenderer("subagent", {
	call: input => {
		const args = parseToolArgs(input);
		const task = args?.task;
		if (typeof task !== "string") return undefined;
		const suffix = typeof args?.cwd === "string" ? toolMuted(` (cwd: ${args.cwd})`) : "";
		return toolTitle(toolBold(`subagent ${task.replace(/\s+/g, " ").trim()}`)) + suffix;
	},
});
