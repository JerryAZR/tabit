/**
 * The entry's resolution ladder — the spike's rule: every leg the entry
 * owns gets a test, because an integration suite that bypasses the entry
 * tests a different program than the one users run.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { resolveBackendCommand } from "../src/cli.ts";

function withEnv<T>(value: string | undefined, run: () => T): T {
	const saved = process.env.TABIT_CORE_BIN;
	if (value === undefined) delete process.env.TABIT_CORE_BIN;
	else process.env.TABIT_CORE_BIN = value;
	try {
		return run();
	} finally {
		if (saved === undefined) delete process.env.TABIT_CORE_BIN;
		else process.env.TABIT_CORE_BIN = saved;
	}
}

describe("the backend resolution ladder", () => {
	test("--mock wins over everything and carries its scenario", () => {
		withEnv("/some/tabit", () => {
			const resolved = resolveBackendCommand(["--mock=tools"], () => true);
			assert.strictEqual(resolved.bin, process.execPath);
			assert.ok(resolved.args?.[0].includes("mock-backend.ts"));
			assert.ok(resolved.args?.join(" ").includes("--scenario tools"));
		});
	});

	test("--bin beats TABIT_CORE_BIN and any sibling build", () => {
		withEnv("/env/tabit", () => {
			const resolved = resolveBackendCommand(["--bin", "/flag/tabit"], () => true);
			assert.strictEqual(resolved.bin, "/flag/tabit");
		});
	});

	test("TABIT_CORE_BIN beats the sibling rung", () => {
		withEnv("/env/tabit", () => {
			const resolved = resolveBackendCommand([], () => true);
			assert.strictEqual(resolved.bin, "/env/tabit");
		});
	});

	test("the sibling rung prefers the gate build, then the plain build", () => {
		withEnv(undefined, () => {
			const devOnly = (path: string): boolean => path.includes("target-test");
			const resolved = resolveBackendCommand([], devOnly);
			assert.ok(resolved.bin.includes("target-test"));
			const resolved2 = resolveBackendCommand([], path => /target[\\/]debug/.test(path));
			assert.strictEqual(/target[\\/]debug/.test(resolved2.bin), true);
		});
	});

	test("the packaged sibling next to the executable is the first file rung", () => {
		withEnv(undefined, () => {
			const resolved = resolveBackendCommand(["--bin-x"], () => false, "C:/packaged/dir");
			assert.strictEqual(resolved.bin, "tabit-core"); // nothing exists anywhere
			const resolved2 = resolveBackendCommand([], path => path.includes("packaged"), "C:/packaged/dir");
			assert.ok(resolved2.bin.includes("packaged")); // execDir rung wins over dev rungs
		});
	});

	test("with nothing local, the rung is `tabit-core` on PATH", () => {
		withEnv(undefined, () => {
			const resolved = resolveBackendCommand([], () => false);
			assert.strictEqual(resolved.bin, "tabit-core");
			assert.deepStrictEqual(resolved.args, ["--json"]); // no args = a NEW session
		});
	});

	test("continuation is explicit: -c / --continue resumes the newest session", () => {
		withEnv(undefined, () => {
			assert.deepStrictEqual(resolveBackendCommand(["-c"], () => false).args, ["--json", "--continue"]);
			assert.deepStrictEqual(resolveBackendCommand(["--continue"], () => false).args, ["--json", "--continue"]);
			assert.deepStrictEqual(resolveBackendCommand([], () => false).args, ["--json"]);
		});
	});
});
