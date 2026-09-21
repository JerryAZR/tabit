/**
 * M0 theme: minimal ANSI-based style functions — the engine requires theme
 * objects at construction, and the semantic-slot system lands in M1. No
 * chalk: raw SGR codes keep the dependency list at the engine alone.
 */

import type { EditorTheme, MarkdownTheme, SelectListTheme } from "@earendil-works/pi-tui";

const dim = (text: string): string => `\x1b[2m${text}\x1b[22m`;
export { dim };
export const accent = (text: string): string => `\x1b[36m${text}\x1b[39m`;
/** Meter thresholds: context badge >70% warns, >90% errors (pi's proven levels). */
export const warn = (text: string): string => `\x1b[33m${text}\x1b[39m`;
export const error = (text: string): string => `\x1b[31m${text}\x1b[39m`;
/** Diff tokens: the edit card's expanded view colors. */
export const diffAdd = (text: string): string => `\x1b[32m${text}\x1b[39m`;
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
