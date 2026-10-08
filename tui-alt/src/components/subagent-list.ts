/**
 * The subagent list — the first widget over the M2 seam (M2-DESIGN.md):
 * the mode projects entries (`{stream, parent, title, state, running,
 * idleSince}`), this widget owns the DISPLAY policy (what shows, how
 * long) and raises intents (`onFocus`). The core never knows which
 * widget is mounted — a picker or a grid replaces this file's class
 * without touching it.
 *
 * First widget's shape (owner ruling): a vertical list below the editor,
 * visible whenever it has rows. Running children always show; idle /
 * failed / aborted rows linger IDLE_HIDE_MS, then hide — hiding never
 * drops a live stream's kept transcript view (the projection and the
 * list are decoupled); a CLOSED stream's hide is different — it arms
 * the mode's sweep, which drops the unreachable state (v23).
 * ↑/↓ move the selection (vertical list — arrows are local
 * to the focused region); Enter focuses the stream; Esc/alt+↑ are the
 * region law's (the input controller's), not the widget's.
 */

import { getKeybindings, matchesKey, type Component } from "@earendil-works/pi-tui";

import { dim, error, success } from "../theme.ts";
import type { SubagentEntry } from "../mode.ts";

/** How long an idle row lingers after its run terminal (owner ruling:
 *  time-based, the simpler of the two offered). */
export const IDLE_HIDE_MS = 60_000;

/** Rows above this scroll a window around the cursor. */
const MAX_ROWS = 5;

export interface SubagentListHost {
	/** Enter on a row: switch stream focus (the mode's focusStream). */
	onFocus(stream: string): void;
	/** The last visible row vanished while the list held region focus —
	 *  the vanished-region law: focus yields the editor. */
	onEmpty(): void;
	/** One or more rows hid this reprojection — the closed-stream sweep's
	 *  hide trigger (v23; the mode re-checks who became unreachable). */
	onHidden(): void;
	requestRender(): void;
}

export class SubagentList implements Component {
	readonly focusable = true;
	/** Whether the list region holds keyboard focus (the root sets it) —
	 *  gates the cursor row's rendering. */
	active = false;
	#entries: SubagentEntry[] = [];
	#visible: SubagentEntry[] = [];
	#cursor = 0;
	#hideTimer: ReturnType<typeof setTimeout> | undefined;
	readonly #host: SubagentListHost;

	constructor(host: SubagentListHost) {
		this.#host = host;
	}

	/** Whether the strip has any row to show (the region law's "when
	 *  present" — an empty list is skipped by alt+↓ and plain ↓). */
	get isEmpty(): boolean {
		return this.#visible.length === 0;
	}

	/** Whether a stream's row is currently visible — the mode's sweep
	 *  query (the widget owns visibility policy; nobody else re-derives
	 *  it). */
	lists(stream: string): boolean {
		return this.#visible.some(e => e.stream === stream);
	}

	setEntries(entries: SubagentEntry[]): void {
		this.#entries = entries;
		this.#reproject();
	}

	dispose(): void {
		if (this.#hideTimer !== undefined) clearTimeout(this.#hideTimer);
	}

	/** Filter to visible rows (running, or idle within the hide window),
	 *  keep the cursor on its entry, and re-arm the hide clock at the
	 *  nearest expiry. Rows that left the visible set raise onHidden. */
	#reproject(): void {
		const now = Date.now();
		const selected = this.#visible[this.#cursor]?.stream;
		const before = new Set(this.#visible.map(e => e.stream));
		this.#visible = this.#entries.filter(e => e.running || e.idleSince === undefined || now - e.idleSince < IDLE_HIDE_MS);
		const hid = before.size > 0 && [...before].some(stream => !this.#visible.some(e => e.stream === stream));
		const kept = selected === undefined ? -1 : this.#visible.findIndex(e => e.stream === selected);
		this.#cursor = kept === -1 ? Math.min(this.#cursor, Math.max(0, this.#visible.length - 1)) : kept;
		if (this.#hideTimer !== undefined) {
			clearTimeout(this.#hideTimer);
			this.#hideTimer = undefined;
		}
		const expiries = this.#visible.filter(e => !e.running && e.idleSince !== undefined).map(e => e.idleSince! + IDLE_HIDE_MS);
		if (expiries.length > 0) {
			const delay = Math.max(1, Math.min(...expiries) - Date.now());
			this.#hideTimer = setTimeout(() => {
				this.#hideTimer = undefined;
				const wasActive = this.active;
				this.#reproject();
				if (wasActive && this.#visible.length === 0) this.#host.onEmpty();
			}, delay);
		}
		if (this.active && this.#visible.length === 0) this.#host.onEmpty();
		if (hid) this.#host.onHidden();
		this.#host.requestRender();
	}

	handleInput(data: string): void {
		const kb = getKeybindings();
		if (matchesKey(data, "up")) {
			this.#cursor = Math.max(0, this.#cursor - 1);
			this.#host.requestRender();
			return;
		}
		if (matchesKey(data, "down")) {
			this.#cursor = Math.min(this.#visible.length - 1, this.#cursor + 1);
			this.#host.requestRender();
			return;
		}
		if (kb.matches(data, "tui.input.submit")) {
			const entry = this.#visible[this.#cursor];
			if (entry !== undefined) this.#host.onFocus(entry.stream);
		}
	}

	render(_width: number): string[] {
		if (this.#visible.length === 0) return [];
		const start = Math.max(0, Math.min(this.#cursor - (MAX_ROWS - 1), this.#visible.length - MAX_ROWS));
		const rows = this.#visible.slice(start, start + MAX_ROWS).map((entry, index) => {
			const cursor = this.active && start + index === this.#cursor ? "❯ " : "  ";
			const dot = entry.running ? success("●") : entry.state === "waiting" || entry.state === "failed" || entry.state === "aborted" ? error("●") : dim("○");
			return ` ${cursor}${dot} ${entry.title} ${dim(`— ${entry.state}`)}`;
		});
		return [dim(" ── subagents " + "─".repeat(20)), ...rows];
	}

	invalidate(): void {}
}
