/**
 * `skill`'s card (owner ruling 2026-10): the call line is `skill
 * <name>`; the collapsed body is the invoked skill's description from
 * the result's `details` cargo (v0.41-era fix on master: the result
 * carries `{ name, description, rel_path, base_dir }` for exactly this)
 * — or, when the entry has no description (a reference material), the
 * path read. A failure (unknown name, unreadable file) renders the
 * default body — the error text IS the answer. The expanded view is the
 * default result law: the full body the model read.
 */

import { defaultResult, parseToolArgs, registerToolCardRenderer, type ToolRenderInput } from "./registry.ts";
import { toolBold, toolMuted, toolTitle } from "../theme.ts";

registerToolCardRenderer("skill", {
	call: input => {
		const name = parseToolArgs(input)?.name;
		if (typeof name !== "string") return undefined;
		return toolTitle(toolBold(`skill ${name}`));
	},
	result: input => {
		if (input.expanded || !input.ok) return defaultResult(input);
		const details = parseSkillDetails(input.details);
		if (details === undefined) return defaultResult(input);
		if (details.description !== undefined) {
			return details.description.split("\n").map(line => toolMuted(line));
		}
		// Reference material carries no description — name the path read.
		return [toolMuted(details.baseDir === undefined ? details.relPath : `${details.baseDir}/${details.relPath}`)];
	},
});

/** The result's `details` cargo, leniently parsed — tool-owned shape
 *  (TOOLS.md); anything unexpected degrades to the default body. */
function parseSkillDetails(details: unknown): { description?: string; relPath: string; baseDir?: string } | undefined {
	if (typeof details !== "object" || details === null) return undefined;
	const record = details as Record<string, unknown>;
	if (typeof record.rel_path !== "string") return undefined;
	return {
		description: typeof record.description === "string" ? record.description : undefined,
		relPath: record.rel_path,
		baseDir: typeof record.base_dir === "string" ? record.base_dir : undefined,
	};
}
