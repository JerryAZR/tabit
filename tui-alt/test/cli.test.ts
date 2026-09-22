/**
 * The entry's resolution ladder — the spike's rule: every leg the entry
 * owns gets a test, because an integration suite that bypasses the entry
 * tests a different program than the one users run.
 */

import { describe, expect, test } from "bun:test";

import { resolveBackendCommand } from "../src/cli";

function withEnv<T>(value: string | undefined, run: () => T): T {
	const saved = process.env.TABIT_BIN;
	if (value === undefined) delete process.env.TABIT_BIN;
	else process.env.TABIT_BIN = value;
	try {
		return run();
	} finally {
		if (saved === undefined) delete process.env.TABIT_BIN;
		else process.env.TABIT_BIN = saved;
	}
}

describe("the backend resolution ladder", () => {
	test("--mock wins over everything and carries its scenario", () => {
		withEnv("/some/tabit", () => {
			const resolved = resolveBackendCommand(["--mock=tools"], () => true);
			expect(resolved.bin).toBe(process.execPath);
			expect(resolved.args?.[0]).toContain("mock-backend.ts");
			expect(resolved.args?.join(" ")).toContain("--scenario tools");
		});
	});

	test("--bin beats TABIT_BIN and any sibling build", () => {
		withEnv("/env/tabit", () => {
			const resolved = resolveBackendCommand(["--bin", "/flag/tabit"], () => true);
			expect(resolved.bin).toBe("/flag/tabit");
		});
	});

	test("TABIT_BIN beats the sibling rung", () => {
		withEnv("/env/tabit", () => {
			const resolved = resolveBackendCommand([], () => true);
			expect(resolved.bin).toBe("/env/tabit");
		});
	});

	test("the sibling rung prefers the gate build, then the plain build", () => {
		withEnv(undefined, () => {
			const devOnly = (path: string): boolean => path.includes("target-test");
			const resolved = resolveBackendCommand([], devOnly);
			expect(resolved.bin).toContain("target-test");
			const resolved2 = resolveBackendCommand([], path => /target[\\/]debug/.test(path));
			expect(/target[\\/]debug/.test(resolved2.bin)).toBe(true);
		});
	});

	test("the packaged sibling next to the executable is the first file rung", () => {
		withEnv(undefined, () => {
			const resolved = resolveBackendCommand(["--bin-x"], () => false, "C:/packaged/dir");
			expect(resolved.bin).toBe("tabit-core"); // nothing exists anywhere
			const resolved2 = resolveBackendCommand([], path => path.includes("packaged"), "C:/packaged/dir");
			expect(resolved2.bin).toContain("packaged"); // execDir rung wins over dev rungs
		});
	});

	test("with nothing local, the rung is `tabit-core` on PATH", () => {
		withEnv(undefined, () => {
			const resolved = resolveBackendCommand([], () => false);
			expect(resolved.bin).toBe("tabit-core");
			expect(resolved.args).toEqual(["--json"]); // no args = a NEW session
		});
	});

	test("continuation is explicit: -c / --continue resumes the newest session", () => {
		withEnv(undefined, () => {
			expect(resolveBackendCommand(["-c"], () => false).args).toEqual(["--json", "--continue"]);
			expect(resolveBackendCommand(["--continue"], () => false).args).toEqual(["--json", "--continue"]);
			expect(resolveBackendCommand([], () => false).args).toEqual(["--json"]);
		});
	});
});
