/**
 * One tool card — pi's ToolExecutionComponent shape (owner ruling 2026-09):
 * a **state-tinted slab** (Box with 1×1 padding; slate while pending, green
 * when done, red when failed) hosting the renderer's call line and result
 * body, with a blank line above so cards breathe. The slab rebuilds its
 * children on every state change (pi's updateDisplay); wrapping and
 * truncation happen at paint inside the engine's Text, so renderers stay
 * width-free. Click (or Ctrl+O) expands a finished card.
 */

import { Box, Container, MouseRegion, Spacer, Text, type Component } from "@earendil-works/pi-tui";

import { TranscriptBlock } from "./transcript-block";
import { rendererFor } from "../tools/renderers";
import { defaultResult, type ToolCardRenderer, type ToolRenderInput } from "../tools/registry";
import { toolErrorBg, toolPendingBg, toolSuccessBg } from "../theme";

export class ToolBlock extends TranscriptBlock {
	readonly #region: MouseRegion;
	/** What the transcript hosts: the breathing line plus the slab, one
	 *  render path for app and tests alike (the lead blank used to live
	 *  only in `render()` — which `asComponent()` bypasses — so adjacent
	 *  cards merged in the app while tests saw a gap). */
	readonly #hosted = new Container();
	readonly #box = new Box(1, 1, toolPendingBg);
	readonly #renderer: Required<ToolCardRenderer>;
	readonly #name: string;
	#args: string | null;
	#content: string | undefined;
	#ok = false;
	#details: unknown;
	#expanded = false;
	/** Call→result duration, client-observed. Undefined when the result
	 *  landed in the same tick (a replay pass) — no fabricated times. */
	readonly #startedAt = Date.now();
	#elapsedMs: number | undefined;

	constructor(turnId: string, requestRender: () => void, name: string, args: string | null) {
		super(turnId, requestRender);
		this.#name = name;
		this.#args = args;
		this.#renderer = rendererFor(name);
		const self = this;
		this.#region = new MouseRegion(this.#box, event => {
			if (event.type === "click" && self.#content !== undefined) {
				self.toggle();
			}
			return undefined;
		});
		this.#hosted.addChild(new Spacer(1));
		this.#hosted.addChild(this.#region);
		this.#refresh();
	}

	setResult(content: string, ok: boolean, details?: unknown): void {
		this.#content = content;
		this.#ok = ok;
		this.#details = details;
		const elapsed = Date.now() - this.#startedAt;
		if (elapsed >= 1) this.#elapsedMs = elapsed;
		this.#refresh();
		this.touch();
	}

	toggle(): void {
		this.#expanded = !this.#expanded;
		this.#refresh();
		this.touch();
	}

	isExpanded(): boolean {
		return this.#expanded;
	}

	setExpanded(expanded: boolean): void {
		this.#expanded = expanded;
		this.#refresh();
		this.touch();
	}

	override asComponent(): Component {
		return this.#hosted;
	}

	override render(width: number): string[] {
		return this.#hosted.render(width);
	}

	override invalidate(): void {
		this.#refresh();
	}

	/** Rebuild the slab's children from current state (pi's updateDisplay). */
	#refresh(): void {
		this.#box.setBgFn(this.#content === undefined ? toolPendingBg : this.#ok ? toolSuccessBg : toolErrorBg);
		this.#box.clear();
		const input: ToolRenderInput = {
			name: this.#name,
			args: this.#args,
			content: this.#content ?? "",
			ok: this.#ok,
			details: this.#details,
			elapsedMs: this.#elapsedMs,
			expanded: this.#expanded,
		};

		this.#box.addChild(new Text(this.#renderer.call(input) ?? this.#name, 0, 0));
		if (this.#content !== undefined) {
			const body = this.#renderer.result(input) ?? defaultResult(input);
			if (body.length > 0) this.#box.addChild(new Text(body.join("\n"), 0, 0));
		}
	}
}
