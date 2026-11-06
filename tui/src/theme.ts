/**
 * The semantic ANSI palette shared by transcript blocks, tool cards,
 * badges, and selectors. No chalk: raw SGR codes keep styling beside the
 * engine without another runtime dependency.
 */

import type { EditorTheme, MarkdownTheme, SelectListTheme } from "@earendil-works/pi-tui";

const dim = (text: string): string => `\x1b[2m${text}\x1b[22m`;
export { dim };
export const accent = (text: string): string => `\x1b[36m${text}\x1b[39m`;
/** Diff tokens: the edit card's expanded view colors. */
export const diffAdd = (text: string): string => `\x1b[32m${text}\x1b[39m`;
/** The green, named for meaning where it is not a diff (tree rows). */
export const success = diffAdd;
export const diffRemove = (text: string): string => `\x1b[31m${text}\x1b[39m`;
export const diffContext = dim;
export const diffHunk = (text: string): string => `\x1b[36m${text}\x1b[39m`;

/**
 * The tool-card palette — pi's dark-theme values, truecolor (the card
 * look is the tinted slab plus its type colors; taking pi's exact values
 * is the point of the port). Title is the near-white `#d4d4d4`, output
 * and muted the calm `#808080`, paths the teal `#8abeb7`; the slabs tint
 * by card state: slate while pending, green when done, red when failed.
 */
const fgRgb = (r: number, g: number, b: number) => (text: string): string => `\x1b[38;2;${r};${g};${b}m${text}\x1b[39m`;
const bgRgb = (r: number, g: number, b: number) => (text: string): string => `\x1b[48;2;${r};${g};${b}m${text}\x1b[49m`;
/** Severity colors (the notice surfaces, meter thresholds, badges):
 *  explicit RGB, not ANSI 31/33 — the ANSI red is terminal-theme-dependent
 *  and typically a dark brick that fails contrast on dark slabs and the
 *  black dock alike. Coral `#f47067` and amber `#e3b341` stay legible on
 *  both. The thresholds stay pi's (context >70% warns, >90% errors). */
export const warn = fgRgb(227, 179, 65);
export const error = fgRgb(244, 112, 103);
export const toolTitle = fgRgb(212, 212, 212);
export const toolOutput = fgRgb(128, 128, 128);
export const toolMuted = toolOutput;
export const toolAccent = fgRgb(138, 190, 183);
export const toolWarn = (text: string): string => `\x1b[33m${text}\x1b[39m`;
export const toolError = (text: string): string => `\x1b[31m${text}\x1b[39m`;
export const toolPendingBg = bgRgb(40, 40, 50);
export const toolSuccessBg = bgRgb(40, 50, 40);
export const toolErrorBg = bgRgb(60, 40, 40);
export const toolBold = (text: string): string => `\x1b[1m${text}\x1b[22m`;

/**
 * The transcript's remaining pi palette: the user message is its own slab
 * (`#343541`, the ChatGPT-lineage gray) with near-white text, and thinking
 * renders italic gray — metadata, not content.
 */
export const userMessageBg = bgRgb(52, 53, 65);
export const userMessageText = fgRgb(212, 212, 212);
/** Transcript notes (lifecycle and error annotations): a neutral slate
 *  slab — deliberately not the user bubble's gray and not a tool card's
 *  state tint, so a note never reads as conversation or as a tool
 *  outcome. Severity speaks through the marker and text, not the slab. */
export const noteBg = bgRgb(36, 36, 42);
/** The compaction row's accent: purple `#bc8cf0`, the one hue the palette
 *  hadn't assigned — a dedicated lifecycle surface, never an info note
 *  (gray) or a severity callout (amber/coral). */
export const compaction = fgRgb(188, 140, 240);
export const italic = (text: string): string => `\x1b[3m${text}\x1b[23m`;
export const thinkingText = (text: string): string => italic(fgRgb(128, 128, 128)(text));
const bold = (text: string): string => `\x1b[1m${text}\x1b[22m`;
const cyan = (text: string): string => `\x1b[36m${text}\x1b[39m`;
const yellow = (text: string): string => `\x1b[33m${text}\x1b[39m`;
const blue = (text: string): string => `\x1b[34m${text}\x1b[39m`;
const green = (text: string): string => `\x1b[32m${text}\x1b[39m`;

export const selectListTheme: SelectListTheme = {
	selectedPrefix: blue,
	selectedText: bold,
	description: dim,
	scrollInfo: dim,
	noMatch: dim,
};

export const markdownTheme: MarkdownTheme = {
	heading: text => bold(cyan(text)),
	link: blue,
	linkUrl: dim,
	code: yellow,
	codeBlock: green,
	codeBlockBorder: dim,
	quote: text => `\x1b[3m${text}\x1b[23m`,
	quoteBorder: dim,
	hr: dim,
	listBullet: cyan,
	bold,
	italic: text => `\x1b[3m${text}\x1b[23m`,
	strikethrough: text => `\x1b[9m${text}\x1b[29m`,
	underline: text => `\x1b[4m${text}\x1b[24m`,
};

export const editorTheme: EditorTheme = {
	borderColor: dim,
	selectList: selectListTheme,
};
