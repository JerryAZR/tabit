#!/usr/bin/env bun
/**
 * Assemble the shipping shape for the host platform into `dist/pkg/`:
 * the Bun-compiled standalone TUI exe (runtime embedded — no JS at run
 * time) plus the cargo release core, side by side — the per-platform
 * package layout the ROADMAP ruling describes — with the npm bin shim.
 *
 * Install the assembled folder with `npm i -g ./dist/pkg` (or bun link).
 * What stays publish-time-only: the registry itself, the meta package's
 * optionalDependencies gating, the cross-target matrix, and signing.
 */

import { spawnSync } from "node:child_process";
import { cpSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import pkg from "../package.json";

const pkgRoot = join(import.meta.dir, "..");
const repoRoot = join(pkgRoot, "..");
const dist = join(pkgRoot, "dist", "pkg");

function run(cmd: string[], opts: { cwd?: string } = {}): void {
	console.log(`+ ${cmd.join(" ")}`);
	const result = spawnSync(cmd[0]!, cmd.slice(1), {
		cwd: opts.cwd,
		stdio: "inherit",
		shell: process.platform === "win32",
	});
	if (result.status !== 0) {
		console.error(`build-release: \`${cmd.join(" ")}\` failed (${result.status})`);
		process.exit(1);
	}
}

const exeSuffix = process.platform === "win32" ? ".exe" : "";
const bunTarget = `bun-${process.platform}-${process.arch}`;

rmSync(dist, { recursive: true, force: true });
mkdirSync(join(dist, "bin"), { recursive: true });

run(["cargo", "build", "--release", "-p", "tabit-core"], { cwd: repoRoot });
run(
	[
		"bun",
		"build",
		"--compile",
		`--target=${bunTarget}`,
		"src/cli.ts",
		`--outfile=${join(dist, `tabit-tui${exeSuffix}`)}`,
	],
	{ cwd: pkgRoot },
);
cpSync(join(repoRoot, "target", "release", `tabit-core${exeSuffix}`), join(dist, `tabit-core${exeSuffix}`));

// The platform package manifest: os/cpu-gated like the real optionalDependency.
writeFileSync(
	join(dist, "package.json"),
	`${JSON.stringify(
		{
			name: "tabit-tui",
			version: pkg.version,
			bin: { "tabit-tui": "./bin/tabit-tui.js" },
			os: [process.platform],
			cpu: [process.arch],
		},
		null,
		2,
	)}\n`,
);

// The only JS that runs on the user's machine: spawn the compiled TUI.
// Everything else lives inside the exe (the embedded runtime + bundle).
writeFileSync(
	join(dist, "bin", "tabit-tui.js"),
	`#!/usr/bin/env node
const { spawn } = require("node:child_process");
const path = require("node:path");
const exe = path.join(__dirname, "..", "tabit-tui" + (process.platform === "win32" ? ".exe" : ""));
const child = spawn(exe, process.argv.slice(2), { stdio: "inherit" });
child.on("error", (err) => {
	process.stderr.write("tabit-tui: cannot launch " + exe + ": " + err.message + "\\n");
	process.exit(1);
});
child.on("exit", (code) => process.exit(code ?? 1));
`,
);

console.log(`assembled ${dist}`);
console.log(`install:  npm i -g "${dist}"`);
