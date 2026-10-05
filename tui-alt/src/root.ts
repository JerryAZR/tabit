/**
 * The alt-screen root: the presentation host. Builds the `TuiAltScreen`
 * layout — the transcript slot over the pinned dock (card slot, queued
 * steering, status strip, editor, subagent list, footer) — and implements
 * `ModeView` by delegating to the component classes under `components/`.
 * The root owns construction, hosting, and focus; each component owns its
 * state, its presentation, and its own repaint request.
 *
 * M2: **one transcript pane per stream** (`StreamPane` = chat container +
 * block registry + scroll view), folded continuously regardless of focus.
 * The transcript slot mounts the focused stream's pane (`showStream` —
 * panes stay alive, so scroll position and collapse state survive a
 * switch); the editor draft is discarded on the switch (pi-tui's editor
 * has no clear-all action to save a stranded one — M2-DESIGN.md).
 *
 * Region focus (the controller's laws, the root's state): the stack is
 * transcript — editor — subagent list. The list region exists only when
 * the widget has rows; a focused region that disappears yields the
 * editor (the vanished-region law, applied in `SubagentList`'s host
 * callbacks and here).
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
import { SubagentList } from "./components/subagent-list.ts";
import { TranscriptRegistry } from "./components/transcript-registry.ts";
import { TreeCardView } from "./components/tree-card.ts";
import { ModelPickerView } from "./model-picker.ts";
import { AuthCardView } from "./auth-card.ts";
import { cardViewFor } from "./card-view.ts";
import { InputController, type Region } from "./input-controller.ts";
import { AtPathCompletionProvider } from "./path-completion.ts";
import { pasteClipboardIntoEditor, platformClipboardReader } from "./paste-image.ts";
import { APP_KEYBINDING_IDS, applyKeybindings, loadTuiToml } from "./keybindings.ts";
import type { FooterFacts, InteractionCard, ModeView, PendingMessage, SkillInfo, SubagentEntry } from "./mode.ts";
import type { InteractiveMode } from "./mode.ts";
import { editorTheme } from "./theme.ts";

/** The type column in the `/` dropdown: fixed-width so descriptions align. */
const TYPE_COLUMN = { command: "command", skill: "skill  " } as const;

/** One stream's transcript: the block stack, its turn index, its scroll
 *  state — kept alive across focus switches (M2-DESIGN.md). */
interface StreamPane {
	chat: Container;
	blocks: TranscriptRegistry;
	scroll: ScrollView;
}

function makePane(): StreamPane {
	const chat = new Container();
	return {
		chat,
		blocks: new TranscriptRegistry(),
		scroll: new ScrollView(chat, {
			follow: "end",
			primary: true,
			overscroll: "chain",
			scrollbar: "auto",
		}),
	};
}

export class AltRoot implements ModeView {
	readonly tui: TuiAltScreen;
	readonly editor: Editor;

	/** The transcript slot: mounts the focused stream's pane. */
	readonly #transcriptSlot = new Container();
	readonly #cardSlot = new Container();
	readonly #pendingQueue: PendingQueue;
	readonly #status: StatusBar;
	readonly #subagentList: SubagentList;
	readonly #footer: FooterBar;
	/** Panes by stream id; the "" key is the pre-boot scratch pane (global
	 *  notes before the root announces), adopted as the root's pane when
	 *  the boot's session_opened lands. */
	readonly #panes = new Map<string, StreamPane>();
	#rootStream: string | undefined;
	#shownStream: string | undefined;
	#region: Region = "editor";
	#mode: InteractiveMode | undefined;
	#input: InputController | undefined;
	/** The open session-tree card, when the tree owns the dock slot. */
	#treeCard: TreeCardView | undefined;
	/** The open model picker, when it owns the dock slot. */
	#modelPicker: ModelPickerView | undefined;
	/** The open login/logout card, when it owns the dock slot. */
	#authCard: AuthCardView | undefined;

	constructor() {
		this.tui = new TuiAltScreen(new ProcessTerminal(), false, undefined, {
			mouse: true,
			scrollToEndIndicator: () => " ↓  jump to latest ",
		});
		this.editor = new Editor(this.tui, editorTheme);
		// File completion roots at the TUI's own cwd until the wire's cwd
		// lands (setFooter's upgrade) — the directory tools run in.
		this.#attachProvider(); // empty until bind — no mode, no command table
		this.#pendingQueue = new PendingQueue(() => this.#touch());
		this.#status = new StatusBar(this.tui, () => this.#touch());
		this.#footer = new FooterBar(() => this.#touch());
		this.#subagentList = new SubagentList({
			onFocus: stream => this.#mode?.focusStream(stream),
			// The vanished-region law: the last row hid while the list held
			// focus — yield the editor.
			onEmpty: () => this.#setRegion("editor"),
			requestRender: () => this.#touch(),
		});
		// The pre-boot scratch pane mounts now; the root's session_opened
		// adopts it (see showStream).
		const scratch = makePane();
		this.#panes.set("", scratch);
		this.#shownStream = "";
		this.#transcriptSlot.addChild(scratch.scroll);
		this.tui.setLayoutRoot(
			new VStack([
				{ component: this.#transcriptSlot, basis: 0, grow: 1, shrink: 1, minSize: 1 },
				{ component: this.#cardSlot, shrink: 1, minSize: 0 },
				{ component: this.#pendingQueue, shrink: 1, minSize: 0 },
				{ component: this.#status, shrink: 1, minSize: 0 },
				{ component: this.editor, shrink: 1, minSize: 3 },
				{ component: this.#subagentList, shrink: 1, minSize: 0 },
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
		for (const warning of warnings) this.addNote(undefined, warning, "warn");
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
		mode.onLogin = () => this.showAuthCard("login");
		mode.onLogout = () => this.showAuthCard("logout");
		// The command table exists now — the dropdown can list it before
		// skills arrive (a skill-less machine still sees the commands).
		this.#attachProvider();
		this.tui.setFocus(this.editor);
		this.#input = new InputController({
			tui: this.tui,
			editor: this.editor,
			isRunning: () => mode.running,
			isCardOpen: () => mode.hasOpenCard || this.#treeCard !== undefined || this.#modelPicker !== undefined || this.#authCard !== undefined,
			interrupt: () => mode.interrupt(),
			escape: () => mode.escape(),
			getRegion: () => this.#region,
			setRegion: region => this.#setRegion(region),
			listVisible: () => !this.#subagentList.isEmpty,
			transcriptScroll: lines => {
				this.#paneFor(this.#shownStream).scroll.scrollBy(lines);
				this.#touch();
			},
			onTree: () => this.showTree(),
			toggleAllCollapsibles: () => {
				// Thinking lines and tool cards together: if any is collapsed,
				// Ctrl+O expands everything, else it collapses everything.
				const blocks = this.#paneFor(this.#shownStream).blocks.collapsibles();
				const anyCollapsed = blocks.some(block => !block.isExpanded());
				for (const block of blocks) block.setExpanded(anyCollapsed);
				this.#touch();
			},
			onPasteImage: () => this.pasteImage(),
			onQuit,
		});
		this.#input.attach();
	}

	dispose(): void {
		this.#input?.detach();
		this.#input = undefined;
		this.#status.dispose();
		this.#footer.dispose();
		this.#subagentList.dispose();
		this.tui.stop();
	}

	#touch(): void {
		this.tui.requestRender();
	}

	// --- regions --------------------------------------------------------------

	/** Apply a region move: the toolkit focus follows (the list is a real
	 *  focusable; the transcript region keeps the toolkit focus on the
	 *  editor — its keys are the controller's intercepts). */
	#setRegion(region: Region): void {
		this.#region = region;
		this.#subagentList.active = region === "list";
		if (region === "list") this.tui.setFocus(this.#subagentList);
		else this.tui.setFocus(this.editor);
		this.#touch();
	}

	// --- stream panes -----------------------------------------------------------

	/** The pane a transcript-bound call folds into (created on demand);
	 *  `undefined` is a backend-level note with no home stream — it lands
	 *  in the root's pane (the scratch pane before the root announces). */
	#paneFor(stream: string | undefined): StreamPane {
		const key = stream ?? this.#rootStream ?? "";
		let pane = this.#panes.get(key);
		if (pane === undefined) {
			pane = makePane();
			this.#panes.set(key, pane);
		}
		return pane;
	}

	// --- ModeView -----------------------------------------------------------

	/** Focus changed (or the root announced): mount the stream's pane and
	 *  discard the editor draft (M2-DESIGN.md's ruling — no clear-all
	 *  exists to save a stranded draft). Region focus yields the editor. */
	showStream(stream: string): void {
		// The first showStream is the root's announce: adopt the scratch
		// pane so pre-boot notes stay where they landed.
		if (this.#rootStream === undefined) {
			this.#rootStream = stream;
			const scratch = this.#panes.get("");
			if (scratch !== undefined && !this.#panes.has(stream)) {
				this.#panes.delete("");
				this.#panes.set(stream, scratch);
			}
		}
		const pane = this.#paneFor(stream);
		if (this.#shownStream !== stream) {
			this.#shownStream = stream;
			this.#transcriptSlot.clear();
			this.#transcriptSlot.addChild(pane.scroll);
			this.editor.setText("");
		}
		this.#setRegion("editor");
		this.#touch();
	}

	beginReplay(stream: string): void {
		const pane = this.#paneFor(stream);
		pane.chat.clear();
		pane.blocks.clear();
		this.#touch();
	}

	endReplay(_stream: string): void {
		this.#touch();
	}

	addUser(stream: string, _entryId: string, text: string): void {
		// Up/down history: user messages OF THE SHOWN STREAM, live or
		// replay-backfilled (pi's shape — in-memory, session-scoped). A
		// child's task text never enters the user's history.
		if (stream === this.#shownStream) this.editor.addToHistory(text);
		this.#paneFor(stream).chat.addChild(new UserBlock(text));
		this.#touch();
	}

	/** The focused stream's skill catalog landed: rebuild the provider from
	 *  the mode's command table — the dropdown is a *view* of that table
	 *  (the kind tag leads the description), never its own list. */
	setSkills(_skills: SkillInfo[]): void {
		this.#attachProvider();
	}

	#attachProvider(): void {
		const entries = (this.#mode?.slashCommands() ?? []).map(entry => ({
			name: entry.name,
			description: `${entry.kind === "skill" ? TYPE_COLUMN.skill : TYPE_COLUMN.command} · ${entry.description}`.trimEnd(),
		}));
		const combined = new CombinedAutocompleteProvider(entries, this.#completionBase);
		this.editor.setAutocompleteProvider(new AtPathCompletionProvider(combined, this.#completionBase));
	}

	/** Ctrl+V (item 4's v0): clipboard image → temp file → an attachment
	 *  tag at the cursor — plain text, expanded by the backend at the
	 *  message door. Text on the clipboard inserts as-is; an empty
	 *  clipboard says so. */
	pasteImage(): void {
		pasteClipboardIntoEditor(text => this.editor.insertTextAtCursor(text), platformClipboardReader())
			.then(outcome => {
				if (outcome.kind === "none") this.addNote(undefined, "nothing to paste — the clipboard holds no image or text", "info");
				this.#touch();
			})
			.catch((error: unknown) => {
				this.addNote(undefined, `clipboard read failed: ${error instanceof Error ? error.message : String(error)}`, "warn");
			});
	}

	/** The completion root: the FOCUSED session's working directory from
	 *  the wire (v16) — a child's spawn cwd differs from the frontend's,
	 *  so process.cwd() is only the pre-boot fallback. Re-attached on
	 *  every footer push whose cwd differs (focus switches included). */
	#completionBase = process.cwd();

	addNote(stream: string | undefined, text: string, kind: "info" | "warn" | "error"): void {
		this.#paneFor(stream).chat.addChild(new NoteBlock(text, kind));
		this.#touch();
	}

	appendAssistantText(stream: string, turnId: string, text: string): void {
		const pane = this.#paneFor(stream);
		let block = pane.blocks.assistant(turnId);
		if (block === undefined) {
			block = new AssistantBlock(turnId, () => this.#touch());
			pane.blocks.putAssistant(turnId, block);
			pane.chat.addChild(block.asComponent());
			this.#touch();
		}
		block.append(text);
	}

	appendReasoning(stream: string, turnId: string, reasoningId: string, text: string): void {
		const pane = this.#paneFor(stream);
		let block = pane.blocks.reasoning(turnId, reasoningId);
		if (block === undefined) {
			block = new ReasoningBlock(turnId, () => this.#touch());
			pane.blocks.putReasoning(turnId, reasoningId, block);
			pane.chat.addChild(block.asComponent());
			this.#touch();
		}
		block.append(text);
	}

	addTool(stream: string, turnId: string, internalCallId: string, name: string, args: string | null): void {
		const pane = this.#paneFor(stream);
		const block = new ToolBlock(turnId, () => this.#touch(), name, args);
		pane.blocks.putTool(turnId, internalCallId, block);
		pane.chat.addChild(block.asComponent());
		this.#touch();
	}

	setToolResult(stream: string, internalCallId: string, content: string, ok: boolean, details?: unknown): void {
		this.#paneFor(stream).blocks.tool(internalCallId)?.setResult(content, ok, details);
		this.#touch();
	}

	removeTurn(stream: string, turnId: string): void {
		const pane = this.#paneFor(stream);
		for (const entry of pane.blocks.removeTurn(turnId)) {
			pane.chat.removeChild(entry.component);
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
		// The focused session's own cwd: the completion root follows it.
		if (facts.cwd !== undefined && facts.cwd !== this.#completionBase) {
			this.#completionBase = facts.cwd;
			this.#attachProvider();
		}
	}

	setSubagents(entries: SubagentEntry[]): void {
		this.#subagentList.setEntries(entries);
	}

	showCard(card: InteractionCard, streamLabel: string | undefined): void {
		// The ask displaces an open tree card (the run is blocked; the tree
		// reopens with ctrl+t). Without this the gate would keep standing
		// down after the card closes — the tree field outlived its slot.
		this.#treeCard = undefined;
		this.#modelPicker = undefined;
		this.#authCard = undefined;
		this.#cardSlot.clear();
		this.#cardSlot.addChild(cardViewFor(card, (selected, text) => this.#mode?.answerCard(card.id, selected, text), streamLabel));
		this.tui.setFocus(this.#cardSlot.children[0]!);
		this.#touch();
	}

	closeCard(id: string, note: string | undefined): void {
		if (note !== undefined) this.addNote(undefined, note, "info");
		this.#cardSlot.clear();
		this.tui.setFocus(this.editor);
		this.#touch();
	}

	// --- session tree ---------------------------------------------------------

	/** Open the tree card (ctrl+t or `/tree`) over the FOCUSED stream's
	 *  tree. A pending interaction card keeps the slot — it owns the run;
	 *  the tree can wait. */
	showTree(): void {
		const mode = this.#mode;
		if (mode === undefined) return;
		if (mode.hasOpenCard) {
			this.addNote(undefined, "answer the open question first — the tree can wait", "warn");
			return;
		}
		if (this.#treeCard !== undefined) return;
		// The tree takes the slot: clear any sibling owner, or its field
		// outlives the slot it no longer renders in (the showCard lesson).
		this.#modelPicker = undefined;
		this.#authCard = undefined;
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
			this.addNote(undefined, "answer the open question first — the picker can wait", "warn");
			return;
		}
		if (this.#modelPicker !== undefined) return;
		if (mode.modelsCatalog.length === 0) {
			this.addNote(undefined, "no usable models at this backend — configure providers or log in first", "warn");
			return;
		}
		this.#treeCard = undefined;
		this.#authCard = undefined;
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

	// --- login / logout --------------------------------------------------------

	/** Open the auth card over the provider status fold (v22): login lists
	 *  the `auth: "none"` targets, logout the `auth: "stored"` rows. A
	 *  pending interaction card keeps the slot. */
	showAuthCard(kind: "login" | "logout"): void {
		const mode = this.#mode;
		if (mode === undefined) return;
		if (mode.hasOpenCard) {
			this.addNote(undefined, "answer the open question first — auth can wait", "warn");
			return;
		}
		if (this.#authCard !== undefined) return;
		const statuses = mode.providerStatuses;
		if (statuses.length === 0) {
			this.addNote(undefined, "no providers configured at this backend — write ~/.tabit/providers.toml and restart", "warn");
			return;
		}
		if (kind === "login" && !statuses.some(s => s.auth === "none")) {
			this.addNote(undefined, "every configured provider has a key source — nothing to log in to", "info");
			return;
		}
		if (kind === "logout" && !statuses.some(s => s.auth === "stored")) {
			this.addNote(undefined, "no stored keys — nothing to log out", "info");
			return;
		}
		this.#treeCard = undefined;
		this.#modelPicker = undefined;
		this.#authCard = new AuthCardView(
			kind,
			statuses,
			{
				onLogin: (provider, apiKey) => {
					this.closeAuthCard();
					mode.login(provider, apiKey);
					this.addNote(undefined, `key stored for ${provider} — the catalog ack confirms`, "info");
				},
				onLogout: provider => {
					this.closeAuthCard();
					mode.logout(provider);
				},
				onClose: () => this.closeAuthCard(),
			},
			() => this.#touch(),
		);
		this.#cardSlot.clear();
		this.#cardSlot.addChild(this.#authCard);
		this.tui.setFocus(this.#authCard);
		this.#touch();
	}

	closeAuthCard(): void {
		if (this.#authCard === undefined) return;
		this.#authCard = undefined;
		this.#cardSlot.clear();
		this.tui.setFocus(this.editor);
		this.#touch();
	}
}
