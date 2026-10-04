/**
 * The alt-screen root: the presentation host. Builds the `TuiAltScreen`
 * layout — transcript `ScrollView` over the pinned dock (card slot, queued
 * steering, status strip, editor, footer) — and implements `ModeView` by
 * delegating to the component classes under `components/`. The root owns
 * construction, hosting, and focus; each component owns its state, its
 * presentation, and its own repaint request.
 *
 * Transcript structure: a flat, append-only stack of blocks created on
 * demand by their first content (never pre-allocated), so document order
 * is wire arrival order; the `TranscriptRegistry` indexes blocks under
 * their turns (keys scoped as narrowly as the protocol scopes them) for
 * whole-group removal on `turn_retried`. Keyboard dispatch lives in the
 * InputController; block rendering lives in the block classes.
 */

import {
	CombinedAutocompleteProvider,
	Container,
	Editor,
	ProcessTerminal,
	ScrollView,
	TuiAltScreen,
	VStack,
	type Component,
} from "@earendil-works/pi-tui";

import { AssistantBlock } from "./components/assistant-block.ts";
import { FooterBar } from "./footer/footer-bar.ts";
import { NoteBlock } from "./components/note-block.ts";
import { PendingQueue } from "./components/pending-queue.ts";
import { ReasoningBlock } from "./components/reasoning-block.ts";
import { StatusBar } from "./components/status-bar.ts";
import { ToolBlock } from "./components/tool-block.ts";
import { UserBlock } from "./components/user-block.ts";
import { TranscriptRegistry } from "./components/transcript-registry.ts";
import { TreeCardView } from "./components/tree-card.ts";
import { ModelPickerView } from "./model-picker.ts";
import { cardViewFor } from "./card-view.ts";
import { InputController } from "./input-controller.ts";
import { AtPathCompletionProvider } from "./path-completion.ts";
import { APP_KEYBINDING_IDS, applyKeybindings, loadTuiToml } from "./keybindings.ts";
import type { FooterFacts, InteractionCard, ModeView, PendingMessage, SkillInfo } from "./mode.ts";
import type { InteractiveMode } from "./mode.ts";
import { editorTheme } from "./theme.ts";

/** The type column in the `/` dropdown: fixed-width so descriptions align. */
const TYPE_COLUMN = { command: "command", skill: "skill  " } as const;

export class AltRoot implements ModeView {
	readonly tui: TuiAltScreen;
	readonly editor: Editor;

	readonly #chat = new Container();
	readonly #cardSlot = new Container();
	readonly #pendingQueue: PendingQueue;
	readonly #status: StatusBar;
	readonly #footer: FooterBar;
	readonly #blocks = new TranscriptRegistry();
	#mode: InteractiveMode | undefined;
	#input: InputController | undefined;
	/** The open session-tree card, when the tree owns the dock slot. */
	#treeCard: TreeCardView | undefined;
	/** The open model picker, when it owns the dock slot. */
	#modelPicker: ModelPickerView | undefined;

	constructor() {
		this.tui = new TuiAltScreen(new ProcessTerminal(), false, undefined, {
			mouse: true,
			scrollToEndIndicator: () => " ↓  jump to latest ",
		});
		this.editor = new Editor(this.tui, editorTheme);
		// File completion roots at the TUI's own cwd — the directory the
		// backend was spawned in, where tools run. (session_opened.path is
		// the session *log file*, not a working directory — data for the
		// future session UI, never a completion root.)
		this.#attachProvider(); // empty until bind — no mode, no command table
		this.#pendingQueue = new PendingQueue(() => this.#touch());
		this.#status = new StatusBar(this.tui, () => this.#touch());
		this.#footer = new FooterBar(() => this.#touch());
		const chatScroll = new ScrollView(this.#chat, {
			follow: "end",
			primary: true,
			overscroll: "chain",
			scrollbar: "auto",
		});
		this.tui.setLayoutRoot(
			new VStack([
				{ component: chatScroll, basis: 0, grow: 1, shrink: 1, minSize: 1 },
				{ component: this.#cardSlot, shrink: 1, minSize: 0 },
				{ component: this.#pendingQueue, shrink: 1, minSize: 0 },
				{ component: this.#status, shrink: 1, minSize: 0 },
				{ component: this.editor, shrink: 1, minSize: 3 },
				{ component: this.#footer, shrink: 1, minSize: 1 },
			]),
		);
		this.setStatus("connecting…");
	}

	/** Wire the mode after construction; the editor's submit path needs it.
	 *  Installs the keybinding registry (tui.toml overrides applied) before
	 *  any input listener attaches. */
	bind(mode: InteractiveMode, onQuit: () => void): void {
		this.#mode = mode;
		const { config, warnings } = loadTuiToml();
		const manager = applyKeybindings(config);
		for (const warning of warnings) this.addNote(warning, "warn");
		mode.setKeybindings(
			APP_KEYBINDING_IDS.map(id => ({
				action: id.replace("tui.app.", ""),
				keys: manager.getKeys(id),
				description: manager.getDefinition(id).description ?? "",
			})),
		);
		this.editor.onSubmit = (text: string) => mode.submit(text);
		mode.onTree = () => this.showTree();
		mode.onModel = () => this.showModelPicker();
		// The command table exists now — the dropdown can list it before
		// skills arrive (a skill-less machine still sees the commands).
		this.#attachProvider();
		this.tui.setFocus(this.editor);
		this.#input = new InputController({
			tui: this.tui,
			editor: this.editor,
			isRunning: () => mode.running,
			isCardOpen: () => mode.hasOpenCard || this.#treeCard !== undefined || this.#modelPicker !== undefined,
			interrupt: () => mode.interrupt(),
			onTree: () => this.showTree(),
			toggleAllCollapsibles: () => {
				// Thinking lines and tool cards together: if any is collapsed,
				// Ctrl+O expands everything, else it collapses everything.
				const blocks = this.#blocks.collapsibles();
				const anyCollapsed = blocks.some(block => !block.isExpanded());
				for (const block of blocks) block.setExpanded(anyCollapsed);
				this.#touch();
			},
			onQuit,
		});
		this.#input.attach();
	}

	dispose(): void {
		this.#input?.detach();
		this.#input = undefined;
		this.#status.dispose();
		this.#footer.dispose();
		this.tui.stop();
	}

	#touch(): void {
		this.tui.requestRender();
	}

	/** Mount a lazily created block in the transcript (the registry has
	 *  already indexed it under its turn). */
	#host(component: Component): void {
		this.#chat.addChild(component);
		this.#touch();
	}

	// --- ModeView -----------------------------------------------------------

	beginReplay(): void {
		this.#chat.clear();
		this.#blocks.clear();
		this.#touch();
	}

	endReplay(): void {
		this.#touch();
	}

	addUser(_entryId: string, text: string): void {
		// Up/down history: every user message, live or replay-backfilled
		// (pi's shape — in-memory, session-scoped).
		this.editor.addToHistory(text);
		this.#chat.addChild(new UserBlock(text));
		this.#touch();
	}

	/** The skill catalog landed: rebuild the provider from the mode's
	 *  command table — the dropdown is a *view* of that table (typed by
	 *  display-only, the type tag leads the description), never its own
	 *  list. */
	setSkills(_skills: SkillInfo[]): void {
		this.#attachProvider();
	}

	#attachProvider(): void {
		const entries = (this.#mode?.slashCommands() ?? []).map(entry => ({
			name: entry.name,
			description: `${entry.displayOnly ? TYPE_COLUMN.skill : TYPE_COLUMN.command} · ${entry.description}`.trimEnd(),
		}));
		const combined = new CombinedAutocompleteProvider(entries, this.#completionBase);
		this.editor.setAutocompleteProvider(new AtPathCompletionProvider(combined, this.#completionBase));
	}

	/** The completion root: the active session's working directory from
	 *  the wire (v16) — a child's spawn cwd differs from the frontend's,
	 *  so process.cwd() is only the pre-boot fallback. */
	#completionBase = process.cwd();

	addNote(text: string, kind: "info" | "warn" | "error"): void {
		this.#chat.addChild(new NoteBlock(text, kind));
		this.#touch();
	}

	appendAssistantText(turnId: string, text: string): void {
		let block = this.#blocks.assistant(turnId);
		if (block === undefined) {
			block = new AssistantBlock(turnId, () => this.#touch());
			this.#blocks.putAssistant(turnId, block);
			this.#host(block.asComponent());
		}
		block.append(text);
	}

	appendReasoning(turnId: string, reasoningId: string, text: string): void {
		let block = this.#blocks.reasoning(turnId, reasoningId);
		if (block === undefined) {
			block = new ReasoningBlock(turnId, () => this.#touch());
			this.#blocks.putReasoning(turnId, reasoningId, block);
			this.#host(block.asComponent());
		}
		block.append(text);
	}

	addTool(turnId: string, internalCallId: string, name: string, args: string | null): void {
		const block = new ToolBlock(turnId, () => this.#touch(), name, args);
		this.#blocks.putTool(turnId, internalCallId, block);
		this.#host(block.asComponent());
	}

	setToolResult(internalCallId: string, content: string, ok: boolean, details?: unknown): void {
		this.#blocks.tool(internalCallId)?.setResult(content, ok, details);
	}

	removeTurn(turnId: string): void {
		for (const entry of this.#blocks.removeTurn(turnId)) {
			this.#chat.removeChild(entry.component);
		}
		this.#touch();
	}

	setPending(pending: PendingMessage[]): void {
		this.#pendingQueue.set(pending);
	}

	setStatus(text: string): void {
		this.#status.set(text);
	}

	setFooter(facts: FooterFacts): void {
		this.#footer.set(facts);
		// The session's own cwd arrived: upgrade the completion root (and
		// again whenever a different session becomes active — M2's child
		// focus will rely on this).
		if (facts.cwd !== undefined && facts.cwd !== this.#completionBase) {
			this.#completionBase = facts.cwd;
			this.#attachProvider();
		}
	}

	showCard(card: InteractionCard): void {
		// The ask displaces an open tree card (the run is blocked; the tree
		// reopens with ctrl+t). Without this the gate would keep standing
		// down after the card closes — the tree field outlived its slot.
		this.#treeCard = undefined;
		this.#modelPicker = undefined;
		this.#cardSlot.clear();
		this.#cardSlot.addChild(cardViewFor(card, (selected, text) => this.#mode?.answerCard(card.id, selected, text)));
		this.tui.setFocus(this.#cardSlot.children[0]!);
		this.#touch();
	}

	closeCard(id: string, note: string | undefined): void {
		if (note !== undefined) this.addNote(note, "info");
		this.#cardSlot.clear();
		this.tui.setFocus(this.editor);
		this.#touch();
	}

	// --- session tree ---------------------------------------------------------

	/** Open the tree card (ctrl+t or `/tree`). A pending interaction card
	 *  keeps the slot — it owns the run; the tree can wait. */
	showTree(): void {
		const mode = this.#mode;
		if (mode === undefined) return;
		if (mode.hasOpenCard) {
			this.addNote("answer the open question first — the tree can wait", "warn");
			return;
		}
		if (this.#treeCard !== undefined) return;
		this.#treeCard = new TreeCardView(
			mode.tree,
			{
				onCheckout: entryId => {
					this.closeTree();
					mode.checkout(entryId);
				},
				onClose: () => this.closeTree(),
			},
			() => this.#touch(),
		);
		this.#cardSlot.clear();
		this.#cardSlot.addChild(this.#treeCard);
		this.tui.setFocus(this.#treeCard);
		this.#touch();
	}

	closeTree(): void {
		if (this.#treeCard === undefined) return;
		this.#treeCard = undefined;
		this.#cardSlot.clear();
		this.tui.setFocus(this.editor);
		this.#touch();
	}

	// --- model picker ---------------------------------------------------------

	/** Open the `/model` picker over the announced catalog (v21). A pending
	 *  interaction card keeps the slot — it owns the run; an empty catalog
	 *  is the setup state, noted instead of opened on. */
	showModelPicker(): void {
		const mode = this.#mode;
		if (mode === undefined) return;
		if (mode.hasOpenCard) {
			this.addNote("answer the open question first — the picker can wait", "warn");
			return;
		}
		if (this.#modelPicker !== undefined) return;
		if (mode.modelsCatalog.length === 0) {
			this.addNote("no usable models at this backend — configure providers or log in first", "warn");
			return;
		}
		this.#treeCard = undefined;
		this.#modelPicker = new ModelPickerView(
			mode.modelsCatalog,
			mode.currentSelection,
			{
				onSelect: (provider, model) => {
					this.closeModelPicker();
					mode.switchModel(provider, model);
				},
				onClose: () => this.closeModelPicker(),
			},
			() => this.#touch(),
		);
		this.#cardSlot.clear();
		this.#cardSlot.addChild(this.#modelPicker);
		this.tui.setFocus(this.#modelPicker);
		this.#touch();
	}

	closeModelPicker(): void {
		if (this.#modelPicker === undefined) return;
		this.#modelPicker = undefined;
		this.#cardSlot.clear();
		this.tui.setFocus(this.editor);
		this.#touch();
	}
}
