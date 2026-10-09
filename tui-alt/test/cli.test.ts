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
	const noPackage = () => undefined;

	test("--mock wins over everything and carries its scenario", () => {
		withEnv("/some/tabit", () => {
			const resolved = resolveBackendCommand(["--mock=tools"], { exists: () => true, resolvePackage: () => "/pkg" });
			assert.strictEqual(resolved.bin, process.execPath);
			assert.ok(resolved.args?.[0].includes("mock-backend.ts"));
			assert.ok(resolved.args?.join(" ").includes("--scenario tools"));
		});
	});

	test("--bin beats TABIT_CORE_BIN and every resolved rung", () => {
		withEnv("/env/tabit", () => {
			const resolved = resolveBackendCommand(["--bin", "/flag/tabit"], { exists: () => true, resolvePackage: () => "/pkg" });
			assert.strictEqual(resolved.bin, "/flag/tabit");
		});
	});

	test("TABIT_CORE_BIN beats the platform package", () => {
		withEnv("/env/tabit", () => {
			const resolved = resolveBackendCommand([], { exists: () => true, resolvePackage: () => "/pkg" });
			assert.strictEqual(resolved.bin, "/env/tabit");
		});
	});

	test("the platform package rung (the published app): the lockstepped core inside it", () => {
		withEnv(undefined, () => {
			const asked: string[] = [];
			const resolved = resolveBackendCommand([], {
				exists: () => true,
				resolvePackage: name => {
					asked.push(name);
					return "/pkg/dir";
				},
			});
			assert.deepStrictEqual(asked, [`tabit-agent-${process.platform}-${process.arch}`]);
			assert.ok(resolved.bin.replaceAll("\\", "/").startsWith("/pkg/dir/"));
			assert.ok(resolved.bin.includes("tabit-core"));
		});
	});

	test("the release-archive rung: the core next to the entry, behind the platform package", () => {
		withEnv(undefined, () => {
			const siblingOnly = (path: string): boolean => /src[\\/]tabit-core/.test(path);
			const resolved = resolveBackendCommand([], { exists: siblingOnly, resolvePackage: noPackage });
			assert.ok(/src[\\/]tabit-core/.test(resolved.bin));
			// The package rung outranks it when both resolve.
			const both = resolveBackendCommand([], { exists: () => true, resolvePackage: () => "/pkg/dir" });
			assert.ok(both.bin.replaceAll("\\", "/").startsWith("/pkg/dir/"));
		});
	});

	test("the dev rungs prefer the gate build, then the plain build", () => {
		withEnv(undefined, () => {
			const gateOnly = (path: string): boolean => path.includes("target-test");
			const resolved = resolveBackendCommand([], { exists: gateOnly, resolvePackage: noPackage });
			assert.ok(resolved.bin.includes("target-test"));
			const plain = resolveBackendCommand([], { exists: path => /target[\\/]debug/.test(path), resolvePackage: noPackage });
			assert.strictEqual(/target[\\/]debug/.test(plain.bin), true);
		});
	});

	test("with nothing local, the rung is `tabit-core` on PATH (a cargo install)", () => {
		withEnv(undefined, () => {
			const resolved = resolveBackendCommand([], { exists: () => false, resolvePackage: noPackage });
			assert.strictEqual(resolved.bin, "tabit-core");
			assert.deepStrictEqual(resolved.args, ["--json"]); // no args = a NEW session
		});
	});

	test("continuation is explicit: -c / --continue resumes the newest session", () => {
		withEnv(undefined, () => {
			assert.deepStrictEqual(resolveBackendCommand(["-c"], { exists: () => false, resolvePackage: noPackage }).args, ["--json", "--continue"]);
			assert.deepStrictEqual(resolveBackendCommand(["--continue"], { exists: () => false, resolvePackage: noPackage }).args, ["--json", "--continue"]);
			assert.deepStrictEqual(resolveBackendCommand([], { exists: () => false, resolvePackage: noPackage }).args, ["--json"]);
		});
	});
});
