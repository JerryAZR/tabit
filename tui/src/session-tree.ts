/**
 * The session tree, built client-side from the wire (owner ruling 2026-09:
 * the frontend constructs the tree; an explicit checkout/branch enumeration
 * lane is a backend decision that may come later). The construction needs
 * no backend help: entry ids are stable across live and replay ("same
 * value by construction" — the replay projects the log's own entry ids),
 * so parenting every newly seen entry on the current chain head, and
 * moving the head on `checked_out.entry_id`, accumulates every branch this
 * process witnessed. A fresh attach sees only what its replay announced —
 * one chain, rendered as a degenerate tree — until the backend names
 * heads explicitly.
 *
 * Chain nodes are exactly the wire events that carry entry ids: user
 * messages, turns (`turn_started`'s id IS the entry id), tool results,
 * compaction passes. Each add* method is idempotent per id: a node seen
 * again (a post-checkout replay re-announcing the shared prefix) advances
 * the head without re-creating or re-parenting — that walk is what makes
 * the replay pass the head-resync path. Turn previews grow only while the
 * turn is open (creation → `turn_retried`/next turn), so a replayed
 * committed turn never doubles its text.
 *
 * Rendering data goes out through `rows()`: a depth-first flattening with
 * pi's tree-selector visual structure — the subtree containing the head
 * sorts first among siblings, single-child chains stay flat, branch points
 * indent — plus the active-path marking the view paints.
 */

export type TreeEntryKind = "user" | "turn" | "tool" | "compaction";

interface TreeNode {
	kind: TreeEntryKind;
	parent: string | null;
	/** Child ids in arrival order. */
	children: string[];
	/** Single-line content snippet — user text, turn text, tool call, or
	 *  compaction size. One line is the contract: the card's line
	 *  accounting breaks on embedded newlines (rows painted past their
	 *  slot, fragments overprinting). Every write path single-lines. */
	preview: string;
}

/** One flattened row: a node plus the prefix structure the view paints. */
export interface TreeRow {
	id: string;
	kind: TreeEntryKind;
	preview: string;
	/** Branch depth — 0 for root entries, +1 at branch points and their first generation. */
	indent: number;
	/** This row carries a branch connector (`├─`/`└─`) — always true off the root level. */
	showConnector: boolean;
	/** This row closes its branch (`└─`) rather than continues it (`├─`). */
	isLast: boolean;
	/** Ancestor levels with a hanging connector: `│` where `show`, blank where not
	 *  (a `└─` ancestor ends its vertical line). */
	gutters: Array<{ position: number; show: boolean }>;
	/** On the root→head path — the chain the session will continue from. */
	onActivePath: boolean;
	/** The chain head itself — checkout here is a no-op. */
	isHead: boolean;
}

const PREVIEW_LIMIT = 200;

export class SessionTree {
	readonly #nodes = new Map<string, TreeNode>();
	/** Arrival-ordered roots (one per fresh session; more only if a future
	 *  wire shape ever emits disjoint chains). */
	readonly #roots: string[] = [];
	/** The current chain end — the parent of the next new entry. */
	#head: string | null = null;
	/** The turn accepting preview text (one at a time; the wire is serial). */
	#openTurn: string | null = null;
	/** Tool calls by internal id: the label a tool row renders from (the
	 *  result event carries the entry id, the call event carries the args). */
	readonly #calls = new Map<string, { name: string; args: string | null }>();

	reset(): void {
		this.#nodes.clear();
		this.#roots.length = 0;
		this.#head = null;
		this.#openTurn = null;
		this.#calls.clear();
	}

	get size(): number {
		return this.#nodes.size;
	}

	get headId(): string | null {
		return this.#head;
	}

	addUser(entryId: string, text: string): void {
		this.#enter(entryId, "user", singleLine(text));
	}

	openTurn(turnId: string): void {
		// A known turn is a replay re-walk: #enter advances the head, but
		// the turn stays closed — its preview is already complete.
		const known = this.#nodes.has(turnId);
		this.#enter(turnId, "turn", "");
		if (!known) this.#openTurn = turnId;
	}

	/** Preview text only reaches an open turn — a replayed committed turn
	 *  (already carrying its full preview) is closed and never doubles.
	 *  Newlines collapse to spaces (one-line contract) but per-append trim
	 *  would eat the stream's own inter-delta spacing — only the start
	 *  trims (a turn may open on blank lines). */
	appendTurnText(turnId: string, text: string): void {
		if (this.#openTurn !== turnId) return;
		const node = this.#nodes.get(turnId);
		if (node === undefined) return;
		node.preview = clip((node.preview + text.replace(/[\n\t]/g, " ")).trimStart());
	}

	noteToolCall(internalCallId: string, name: string, args: string | null): void {
		this.#calls.set(internalCallId, { name, args });
	}

	addTool(entryId: string, internalCallId: string, name: string, content: string): void {
		const call = this.#calls.get(internalCallId);
		this.#calls.delete(internalCallId);
		const label = call === undefined ? name : formatCall(call.name, call.args);
		void content; // the result body is the transcript's job; the row shows the call
		this.#enter(entryId, "tool", label);
	}

	addCompaction(id: string, totalTokens: number): void {
		this.#enter(id, "compaction", `[compaction: ${Math.round(totalTokens / 1000)}k tokens]`);
	}

	/** A discarded attempt: the turn never persisted, and its tool results
	 *  (its only possible children) go with it. If the head died with the
	 *  subtree it falls back to the surviving parent. */
	retryTurn(turnId: string): void {
		if (this.#openTurn === turnId) this.#openTurn = null;
		const node = this.#nodes.get(turnId);
		if (node === undefined) return;
		this.#dropSubtree(turnId);
		if (node.parent !== null) {
			const parent = this.#nodes.get(node.parent);
			if (parent !== undefined) parent.children = parent.children.filter(id => id !== turnId);
		} else {
			const rootIndex = this.#roots.indexOf(turnId);
			if (rootIndex >= 0) this.#roots.splice(rootIndex, 1);
		}
		if (this.#head !== null && !this.#nodes.has(this.#head)) this.#head = node.parent;
	}

	/** Nothing streams during a replay pass and its announcements are
	 *  finalized history — the open turn (if any) closes so a re-announce
	 *  never doubles its preview. */
	closeTurn(): void {
		this.#openTurn = null;
	}

	/** The chain moved: the head becomes the target entry. Creates nothing —
	 *  the target was announced before (or the following replay walk lands
	 *  it); an unknown head still renders, just without a path past it. */
	checkout(entryId: string): void {
		this.#openTurn = null;
		this.#head = entryId;
	}

	/** Depth-first rows, head's subtree first among siblings — the reading
	 *  order pi's tree uses, so the live branch reads top-down. */
	rows(): TreeRow[] {
		const activePath = this.#activePath();
		const rows: TreeRow[] = [];
		const orderedRoots = this.#activeFirst([...this.#roots], activePath);
		const walk = (
			id: string,
			indent: number,
			justBranched: boolean,
			showConnector: boolean,
			isLast: boolean,
			gutters: Array<{ position: number; show: boolean }>,
			isVirtualRootChild: boolean,
		): void => {
			const node = this.#nodes.get(id)!;
			const children = node.children;
			const multipleChildren = children.length > 1;
			rows.push({
				id,
				kind: node.kind,
				preview: node.preview,
				indent,
				showConnector,
				isLast,
				gutters,
				onActivePath: activePath.has(id),
				isHead: id === this.#head,
			});
			let childIndent: number;
			if (multipleChildren) childIndent = indent + 1;
			else if (justBranched && indent > 0) childIndent = indent + 1;
			else childIndent = indent;
			// The gutter mirrors this row's connector one level out; a closing
			// branch (`└─`) ends its vertical line — descendants render blanks there.
			const childGutters =
				showConnector && !isVirtualRootChild
					? [...gutters, { position: Math.max(0, indent - 1), show: !isLast }]
					: gutters;
			const orderedChildren = this.#activeFirst(children, activePath);
			for (let index = 0; index < orderedChildren.length; index++) {
				const childIsLast = index === orderedChildren.length - 1;
				walk(
					orderedChildren[index]!,
					childIndent,
					multipleChildren,
					multipleChildren,
					childIsLast,
					childGutters,
					false,
				);
			}
		};
		const multipleRoots = this.#roots.length > 1;
		for (let index = 0; index < orderedRoots.length; index++) {
			walk(
				orderedRoots[index]!,
				multipleRoots ? 1 : 0,
				multipleRoots,
				multipleRoots,
				index === orderedRoots.length - 1,
				[],
				multipleRoots,
			);
		}
		return rows;
	}

	/** Create-or-advance: the one entry path. A known id (replay re-walking
	 *  the shared prefix) only moves the head; a new one parents on it. */
	#enter(id: string, kind: TreeEntryKind, preview: string): void {
		const existing = this.#nodes.get(id);
		if (existing !== undefined) {
			this.#head = id;
			return;
		}
		this.#nodes.set(id, { kind, parent: this.#head, children: [], preview });
		if (this.#head === null) this.#roots.push(id);
		else this.#nodes.get(this.#head)!.children.push(id);
		this.#head = id;
	}

	#dropSubtree(id: string): void {
		const node = this.#nodes.get(id);
		if (node === undefined) return;
		for (const child of node.children) this.#dropSubtree(child);
		this.#nodes.delete(id);
	}

	#activePath(): Set<string> {
		const path = new Set<string>();
		let cursor = this.#head;
		while (cursor !== null) {
			path.add(cursor);
			cursor = this.#nodes.get(cursor)?.parent ?? null;
		}
		return path;
	}

	/** Stable reorder: ids whose subtree contains the head first. */
	#activeFirst(ids: string[], activePath: Set<string>): string[] {
		if (ids.length < 2) return ids;
		const contains = (id: string): boolean => {
			if (activePath.has(id)) return true;
			const node = this.#nodes.get(id);
			return node !== undefined && node.children.some(contains);
		};
		return [...ids].sort((a, b) => Number(contains(b)) - Number(contains(a)));
	}
}

function singleLine(text: string): string {
	return clip(text.replace(/[\n\t]/g, " ").trim());
}

function clip(text: string): string {
	return text.length > PREVIEW_LIMIT ? `${text.slice(0, PREVIEW_LIMIT)}…` : text;
}

/** pi's muted tool-call row shape: `name arg value…` from the JSON args.
 *  Values are single-lined — a write call's `content` argument is
 *  multi-line, and a preview row is one line by contract. */
function formatCall(name: string, args: string | null): string {
	if (args === null || args === "") return name;
	try {
		const parsed = JSON.parse(args) as Record<string, unknown>;
		const parts = Object.entries(parsed)
			.slice(0, 3)
			.map(([key, value]) => `${key}: ${singleLine(String(value))}`);
		return singleLine(parts.length > 0 ? `${name} ${parts.join(", ")}` : name);
	} catch {
		return singleLine(`${name} ${clip(args)}`);
	}
}
