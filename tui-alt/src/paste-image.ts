/**
 * Image paste (roadmap item 4's v0): clipboard image → temp file → an
 * `<attachment path="…"/>` tag at the editor cursor. The tag is plain
 * text — chips are deferred — and the backend expands it at the message
 * door, the same mailbox funnel as the skill tag (the backend's
 * expansion is master's gap to fill; the TUI emits the tag today).
 *
 * Acquisition rides pi-tui's bundled native clipboard (macOS/Windows/X11
 * readers) with the Wayland/X11 shell-out fallbacks pi's
 * clipboard-image.ts models. No image on the clipboard falls back to
 * text (pi's Ctrl+V chain: image first, then text). Readers are
 * injectable — tests never touch a real clipboard.
 */

import { execFile } from "node:child_process";
import { writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";

import { getNativeClipboard } from "@earendil-works/pi-tui";

export interface ClipboardReader {
	/** Undefined = unavailable, null = no image; transfer failures reject. */
	readImage(): Promise<Uint8Array | null | undefined>;
	/** Undefined = unavailable, null = no text. */
	readText(): Promise<string | null | undefined>;
}

/** The acquisition chain: pi-tui's bundled native helper, then the
 *  Wayland/X11 command-line tools (Linux retains clipboard ownership in
 *  the source app, so the tools read it synchronously). */
export function platformClipboardReader(): ClipboardReader {
	const native = getNativeClipboard();
	if (native !== undefined) {
		return {
			readImage: () => native.getImage(),
			readText: () => native.getText(),
		};
	}
	if (process.env.WAYLAND_DISPLAY !== undefined) {
		return new CommandReader("wl-paste", {
			types: ["--list-types"],
			read: type => ["--type", type],
			textType: "text/plain",
		});
	}
	if (process.env.DISPLAY !== undefined) {
		return new CommandReader("xclip", {
			types: ["-selection", "clipboard", "-t", "TARGETS", "-o"],
			read: type => ["-selection", "clipboard", "-t", type, "-o"],
			textType: "text/plain",
		});
	}
	return {
		readImage: () => Promise.resolve(undefined),
		readText: () => Promise.resolve(undefined),
	};
}

/** Clipboard reads through a command-line tool (wl-paste / xclip). */
class CommandReader implements ClipboardReader {
	readonly #cmd: string;
	readonly #args: { types: string[]; read: (type: string) => string[]; textType: string };

	constructor(cmd: string, args: { types: string[]; read: (type: string) => string[]; textType: string }) {
		this.#cmd = cmd;
		this.#args = args;
	}

	#run(args: string[]): Promise<Buffer | null> {
		return new Promise(resolve => {
			// A missing tool or an empty clipboard is "nothing here", not an
			// error — the chord simply finds nothing.
			execFile(this.#cmd, args, { encoding: "buffer", maxBuffer: 64 * 1024 * 1024 }, (error, stdout) => {
				resolve(error !== null || stdout.length === 0 ? null : stdout);
			});
		});
	}

	async readImage(): Promise<Uint8Array | null | undefined> {
		const listed = await this.#run(this.#args.types);
		if (listed === null) return undefined;
		const type = listed
			.toString("utf8")
			.split("\n")
			.map(line => line.trim())
			.find(line => line.startsWith("image/"));
		if (type === undefined) return null;
		return this.#run(this.#args.read(type));
	}

	readText(): Promise<string | null | undefined> {
		return this.#run(this.#args.read(this.#args.textType)).then(bytes => bytes?.toString("utf8") ?? null);
	}
}

/** The image format from its magic bytes; undefined = unrecognized
 *  (saved as .png — most clipboard sources speak it). */
export function sniffImageExtension(bytes: Uint8Array): string | undefined {
	const starts = (...magic: number[]) => magic.every((byte, index) => bytes[index] === byte);
	if (starts(0x89, 0x50, 0x4e, 0x47)) return "png";
	if (starts(0xff, 0xd8, 0xff)) return "jpg";
	if (starts(0x47, 0x49, 0x46, 0x38)) return "gif";
	if (starts(0x42, 0x4d)) return "bmp";
	if (starts(0x52, 0x49, 0x46, 0x46) && bytes[8] === 0x57 && bytes[9] === 0x45) return "webp";
	return undefined;
}

/** The default temp writer: `<tmpdir>/tabit-paste-<uuid>.<ext>`. */
export function defaultTempWrite(bytes: Uint8Array, ext: string): string {
	const dir = join(tmpdir(), "tabit-paste");
	mkdirSync(dir, { recursive: true });
	const path = join(dir, `${randomUUID()}.${ext}`);
	writeFileSync(path, bytes);
	return path;
}

export type PasteOutcome = { kind: "image"; path: string } | { kind: "text" } | { kind: "none" };

/** The chord's fold: image → temp file + attachment tag at the cursor;
 *  text → inserted as-is (pi's chain); nothing → "none" for the caller
 *  to report. */
export async function pasteClipboardIntoEditor(
	insert: (text: string) => void,
	reader: ClipboardReader,
	writeTemp: (bytes: Uint8Array, ext: string) => string = defaultTempWrite,
): Promise<PasteOutcome> {
	const image = await reader.readImage();
	if (image != null) {
		const path = writeTemp(image, sniffImageExtension(image) ?? "png");
		insert(`<attachment path="${path}"/>`);
		return { kind: "image", path };
	}
	const text = await reader.readText();
	if (text != null && text !== "") {
		insert(text);
		return { kind: "text" };
	}
	return { kind: "none" };
}
