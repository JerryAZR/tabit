/**
 * The window title's path fold — tail-anchored under a character budget,
 * because the path's tail is what tells tabit windows apart. Every leg
 * of the fold gets a test: verbatim, the boundary, head ellipsis, the
 * separator following the source, and the lone-long-basename fallback.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { windowTitle } from "../src/window-title.ts";

describe("the window title", () => {
	test("no cwd yet: the bare app name", () => {
		assert.equal(windowTitle(undefined), "tabit");
	});

	test("a path inside the budget shows verbatim", () => {
		assert.equal(windowTitle("D:\\Projects\\Agents\\tabit"), "tabit — D:\\Projects\\Agents\\tabit");
	});

	test("the budget boundary is inclusive", () => {
		const exact = "a".repeat(48);
		assert.equal(windowTitle(exact), `tabit — ${exact}`);
	});

	test("an overflow keeps the trailing components, ellipsizing the head", () => {
		const project = "p".repeat(26);
		const cwd = `D:\\Projects\\Agents\\${project}\\worktrees\\feature-x`;
		// `…\<project>\worktrees\feature-x` is exactly the 48-char budget;
		// adding `\Agents` would overflow, so the fold stops there.
		assert.equal(windowTitle(cwd), `tabit — …\\${project}\\worktrees\\feature-x`);
	});

	test("the separator follows the source path", () => {
		const project = "p".repeat(24);
		const cwd = `/home/somebody/projects/${project}/worktrees/feature-x`;
		assert.equal(windowTitle(cwd), `tabit — …/${project}/worktrees/feature-x`);
	});

	test("a basename that alone overflows truncates from the left, never the right", () => {
		const base = "x".repeat(60);
		assert.equal(windowTitle(`D:\\${base}`), `tabit — …${"x".repeat(47)}`);
	});
});
