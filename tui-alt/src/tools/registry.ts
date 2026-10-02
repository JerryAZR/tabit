/**
 * The tool-card renderer mechanism — pi's call/result split (owner
 * ruling 2026-09: "learn how pi implements tool cards"): a card is a
 * state-tinted slab hosting two stacked views, **call** (the invocation,
 * built from args — bold verb + key argument, always visible, collapsed
 * and expanded alike) and **result** (the body, built from the result —
 * themed output, collapsed preview vs expanded full). The split is why
 * pi's cards read cleanly: the invocation line never disappears, and the
 * result decides for itself what a preview is.
 *
 * Every tool gets the default handlers; a tool registers a renderer
 * under its wire name to override. Registration itself lives in
 * `renderers.ts` (the one site).
 */

export interface ToolRenderInput {
	name: string;
	args: string | null;
	content: string;
	ok: boolean;
	/** The tool_result `details` cargo, parsed per renderer (edit hunks…). */
	details?: unknown;
	/** Client-observed call→result duration; absent when the result landed
	 *  in the same tick (a replay pass — the wire carries no durations). */
	elapsedMs?: number;
	/** The card's expanded state (per card: click or Ctrl+O). */
	expanded: boolean;
}

export interface ToolCardRenderer {
	/** The invocation line. Omit for the default (bold tool name). */
	call?(input: ToolRenderInput): string | undefined;
	/** The result body under the call line. Omit — or return `undefined`
	 *  (nothing to show, like a read preview) — for the default (themed
	 *  output, 5 preview lines collapsed, all lines expanded). */
	result?(input: ToolRenderInput): string[] | undefined;
}

const registry = new Map<string, ToolCardRenderer>();

export function registerToolCardRenderer(name: string, renderer: ToolCardRenderer): void {
	registry.set(name, renderer);
}

/** The default call line: bold tool name (pi's call fallback). */
export function defaultCall(input: ToolRenderInput): string {
	return toolTitleBold(input.name);
}

/** The default result body: themed output; preview 5 lines collapsed. */
export function defaultResult(input: ToolRenderInput): string[] {
	const lines = input.content.split("\n");
	const styled = lines.map(line => toolOutputLine(line));
	if (input.expanded) return styled;
	const shown = styled.slice(0, DEFAULT_PREVIEW_LINES);
	const remaining = styled.length - shown.length;
	if (remaining > 0) shown.push(mutedLine(`... (${remaining} more lines, ctrl+o to expand)`));
	return shown;
}

/** Resolve a tool's card renderer, defaults filling the gaps. */
export function rendererFor(name: string): Required<ToolCardRenderer> {
	const renderer = registry.get(name);
	return {
		call: renderer?.call ?? defaultCall,
		result: renderer?.result ?? defaultResult,
	};
}

/**
 * Lenient tool-arguments parse for call lines (bash's `command`, read's
 * `path`, …). Anything but a JSON object comes back undefined — the
 * caller falls back to the default call instead of guessing.
 */
export function parseToolArgs(input: ToolRenderInput): Record<string, unknown> | undefined {
	if (input.args === null) return undefined;
	try {
		const parsed: unknown = JSON.parse(input.args);
		return typeof parsed === "object" && parsed !== null ? (parsed as Record<string, unknown>) : undefined;
	} catch {
		return undefined;
	}
}

const DEFAULT_PREVIEW_LINES = 5;

import { toolBold, toolMuted, toolOutput, toolTitle } from "../theme.ts";

function toolTitleBold(text: string): string {
	return toolTitle(toolBold(text));
}

function toolOutputLine(text: string): string {
	return toolOutput(text);
}

function mutedLine(text: string): string {
	return toolMuted(text);
}
