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
	Container,
	Editor,
	ProcessTerminal,
	ScrollView,
	TuiAltScreen,
	VStack,
	type Component,
} from "@earendil-works/pi-tui";

import { AssistantBlock } from "./components/assistant-block";
import { FooterBar } from "./footer/footer-bar";
import { NoteBlock } from "./components/note-block";
import { PendingQueue } from "./components/pending-queue";
import { ReasoningBlock } from "./components/reasoning-block";
import { StatusBar } from "./components/status-bar";
import { ToolBlock } from "./components/tool-block";
import { UserBlock } from "./components/user-block";
import { TranscriptRegistry } from "./components/transcript-registry";
import { CardView } from "./card-view";
import { InputController } from "./input-controller";
import type { FooterFacts, InteractionCard, ModeView, PendingMessage } from "./mode";
import type { InteractiveMode } from "./mode";
import { editorTheme } from "./theme";

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

	constructor() {
		this.tui = new TuiAltScreen(new ProcessTerminal(), false, undefined, {
			mouse: true,
			scrollToEndIndicator: () => " ↓  jump to latest ",
		});
		this.editor = new Editor(this.tui, editorTheme);
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

	/** Wire the mode after construction; the editor's submit path needs it. */
	bind(mode: InteractiveMode, onQuit: () => void): void {
		this.#mode = mode;
		this.editor.onSubmit = (text: string) => mode.submit(text);
		this.tui.setFocus(this.editor);
		this.#input = new InputController({
			tui: this.tui,
			editor: this.editor,
			isRunning: () => mode.running,
			interrupt: () => mode.interrupt(),
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
		this.#chat.addChild(new UserBlock(text));
		this.#touch();
	}

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
	}

	showCard(card: InteractionCard): void {
		this.#cardSlot.clear();
		this.#cardSlot.addChild(new CardView(card, index => this.#mode?.answerCard(card.id, index)));
		this.tui.setFocus(this.#cardSlot.children[0]!);
		this.#touch();
	}

	closeCard(id: string, note: string | undefined): void {
		if (note !== undefined) this.addNote(note, "info");
		this.#cardSlot.clear();
		this.tui.setFocus(this.editor);
		this.#touch();
	}
}
