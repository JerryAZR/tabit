/**
 * Image paste, headless: the magic-byte sniff and the chord's fold —
 * image → temp file + attachment tag at the cursor, text → inserted
 * as-is, empty clipboard → "none". Readers and the temp writer are
 * injected; no real clipboard, no real files.
 */

import assert from "node:assert/strict";
import { readFileSync, rmSync } from "node:fs";
import { describe, test } from "node:test";

import { defaultTempWrite, pasteClipboardIntoEditor, sniffImageExtension, type ClipboardReader } from "../src/paste-image.ts";

const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
const JPEG = new Uint8Array([0xff, 0xd8, 0xff, 0xe0]);
const GIF = new Uint8Array([0x47, 0x49, 0x46, 0x38, 0x39, 0x61]);
const BMP = new Uint8Array([0x42, 0x4d]);
const WEBP = new Uint8Array([0x52, 0x49, 0x46, 0x46, 0, 0, 0, 0, 0x57, 0x45, 0x42, 0x50]);

const reader = (image: Uint8Array | null | undefined, text: string | null | undefined = null): ClipboardReader => ({
	readImage: () => Promise.resolve(image),
	readText: () => Promise.resolve(text),
});

describe("sniffImageExtension", () => {
	test("the five formats by magic bytes; unknown is undefined", () => {
		assert.strictEqual(sniffImageExtension(PNG), "png");
		assert.strictEqual(sniffImageExtension(JPEG), "jpg");
		assert.strictEqual(sniffImageExtension(GIF), "gif");
		assert.strictEqual(sniffImageExtension(BMP), "bmp");
		assert.strictEqual(sniffImageExtension(WEBP), "webp");
		assert.strictEqual(sniffImageExtension(new Uint8Array([1, 2, 3])), undefined);
	});
});

describe("pasteClipboardIntoEditor", () => {
	test("an image becomes a temp file and an attachment tag at the cursor", async () => {
		const inserted: string[] = [];
		const written: Array<{ bytes: Uint8Array; ext: string }> = [];
		const outcome = await pasteClipboardIntoEditor(
			text => inserted.push(text),
			reader(PNG),
			(bytes, ext) => {
				written.push({ bytes, ext });
				return "/tmp/fake/paste.png";
			},
		);
		assert.deepStrictEqual(outcome, { kind: "image", path: "/tmp/fake/paste.png" });
		assert.deepStrictEqual(inserted, ['<attachment path="/tmp/fake/paste.png"/>']);
		assert.strictEqual(written[0]!.ext, "png"); // sniffed, not assumed
	});

	test("no image falls back to text (pi's chain); empty clipboard is none", async () => {
		const inserted: string[] = [];
		const textOutcome = await pasteClipboardIntoEditor(text => inserted.push(text), reader(null, "hello"), () => "/unused");
		assert.deepStrictEqual(textOutcome, { kind: "text" });
		assert.deepStrictEqual(inserted, ["hello"]);

		const none = await pasteClipboardIntoEditor(() => {}, reader(null, null), () => "/unused");
		assert.deepStrictEqual(none, { kind: "none" });
	});

	test("an unavailable reader (undefined) behaves like an empty clipboard", async () => {
		const unavailable = await pasteClipboardIntoEditor(() => {}, reader(undefined, undefined), () => "/unused");
		assert.deepStrictEqual(unavailable, { kind: "none" });
	});

	test("the temp writer names files with a human-id phrase, not a UUID", () => {
		const path = defaultTempWrite(PNG, "png");
		try {
			assert.match(path, /tabit-paste[/\\][a-z]+-[a-z]+-[a-z]+\.png$/);
			assert.deepStrictEqual(new Uint8Array(readFileSync(path)), PNG);
		} finally {
			rmSync(path);
		}
	});
});
