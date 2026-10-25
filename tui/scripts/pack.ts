#!/usr/bin/env node
/**
 * Assemble the publishable family into `dist/` — the Node-era release
 * shape (owner ruling: ship the `.ts` sources, no compile step; the
 * Bun-compiled standalone exe is dead):
 *
 *   tabit-agent               the app — a single-file esbuild bundle
 *                             (`bin/tabit.js`; plain JS for startup
 *                             speed, sources live on GitHub) with bin
 *                             `tabit`; pi-tui stays external (its napi
 *                             clipboard prebuilds load by path from its
 *                             own package). optionalDependencies pin
 *                             every platform package at the same
 *                             version (the lockstep ruling made atomic)
 *   tabit-agent-<platform>    one cargo release core each, os/cpu-gated
 *
 * Local: `node scripts/pack.ts` builds the host core (`cargo build
 * --release -p tabit-core`) and packs the meta package plus the host's
 * platform package. CI passes one `--core <platform>=<path>` per matrix
 * entry (the per-platform jobs upload, the publish job packs).
 *
 * The meta's optionalDependencies always name the full release MATRIX
 * (the packages a real publish must include); `--matrix win32-x64`
 * overrides it for local testing, where the other platform packages
 * don't exist on the registry. Platform package manifests deliberately
 * carry no `exports` field — the entry resolves `<name>/package.json`
 * through node resolution (the ladder's npm rung).
 *
 * `--publish` uploads the tarballs just packed — platform packages first,
 * the meta last (a fresh install's optional deps must resolve from the
 * registry the moment the meta is visible) — and is the ONE publish path
 * for both flows:
 *
 *   manual   `node scripts/pack.ts --matrix linux-x64 --publish` — rides
 *            the logged-in account's 2FA prompt (stdio is inherited).
 *            The first publish of a package is always this one: npm's
 *            trusted publishing links only an existing package.
 *   CI       `node scripts/pack.ts --core win32-x64=<path> --core
 *            linux-x64=<path> --publish --provenance` — `--provenance`
 *            needs the runner's OIDC (`id-token: write`) and the trusted
 *            publisher configured on npmjs.com; the matrix jobs hand the
 *            cores in, nothing builds here.
 *
 * Without `--publish` the run stops at the tarballs and prints the
 * local-install line — the local loop.
 */

import { spawnSync } from "node:child_process";
import { cpSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { build } from "esbuild";

/** The release matrix (owner ruling): the two testable platforms first;
 *  darwin-arm64 joins after someone tests a mac; the rest are deferred. */
const MATRIX = ["win32-x64", "linux-x64"];

const pkgRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = join(pkgRoot, "..");
const dist = join(pkgRoot, "dist");
const staging = join(dist, "staging");

function run(cmd: string[], opts: { cwd?: string } = {}): void {
	console.log(`+ ${cmd.join(" ")}`);
	// npm is npm.cmd on Windows, and Node refuses to spawn .cmd without a
	// shell (CVE-2024-27980) — the DEP0190 warning is the cost; the args
	// here are fixed strings, never user input.
	const result = spawnSync(cmd[0]!, cmd.slice(1), {
		cwd: opts.cwd,
		stdio: "inherit",
		shell: process.platform === "win32",
	});
	if (result.status !== 0) {
		console.error(`pack: \`${cmd.join(" ")}\` failed (${result.status})`);
		process.exit(1);
	}
}

// --- args ------------------------------------------------------------------

const argv = process.argv.slice(2);
/** platform token → core binary path (CI's artifact handoff). */
const cores = new Map<string, string>();
let matrix = MATRIX;
let publish = false;
let provenance = false;
for (let i = 0; i < argv.length; i++) {
	if (argv[i] === "--core") {
		const spec = argv[++i] ?? "";
		const eq = spec.indexOf("=");
		if (eq === -1) {
			console.error(`pack: --core wants <platform>=<path>, got "${spec}"`);
			process.exit(1);
		}
		cores.set(spec.slice(0, eq), spec.slice(eq + 1));
	} else if (argv[i] === "--matrix") {
		matrix = (argv[++i] ?? "").split(",").filter(s => s !== "");
	} else if (argv[i] === "--publish") {
		publish = true;
	} else if (argv[i] === "--provenance") {
		provenance = true;
	} else {
		console.error(`pack: unknown argument ${argv[i]} (usage: [--core <platform>=<path>]... [--matrix a,b] [--publish] [--provenance])`);
		process.exit(1);
	}
}
if (provenance && !publish) {
	console.error("pack: --provenance only rides --publish");
	process.exit(1);
}

const meta = JSON.parse(readFileSync(join(pkgRoot, "package.json"), "utf8")) as {
	name: string;
	version: string;
	[key: string]: unknown;
};
if (meta.name !== "tabit-agent") {
	console.error(`pack: the meta package is ${meta.name}, expected tabit-agent`);
	process.exit(1);
}
const version = meta.version;

// The platforms we pack this run: every --core entry, plus the host when
// no core was handed in for it (the local loop builds it).
const host = `${process.platform}-${process.arch}`;
const toPack = new Set(cores.keys());
if (!cores.has(host)) toPack.add(host);

// --- build / collect cores ---------------------------------------------------

if (!cores.has(host)) {
	run(["cargo", "build", "--release", "-p", "tabit-core"], { cwd: repoRoot });
	const exe = process.platform === "win32" ? "tabit-core.exe" : "tabit-core";
	cores.set(host, join(repoRoot, "target", "release", exe));
}

// --- stage + pack --------------------------------------------------------------

rmSync(staging, { recursive: true, force: true });
mkdirSync(staging, { recursive: true });

// The meta package: the bundle + the manifest. The bundle is one file
// (esbuild, ESM, node24) — pi-tui external (its napi prebuilds load by
// path from its own package), the tiny pure-JS deps (human-id) baked in.
// The manifest is rewritten for the artifact: the entry moves to the
// bundle, files shrinks to it, human-id leaves `dependencies` (bundled),
// and the lockstepped optionalDependencies are injected at pack time —
// the committed manifest stays free of them so dev `npm install` never
// 404s on unpublished platform packages.
const metaDir = join(staging, "tabit-agent");
mkdirSync(join(metaDir, "bin"), { recursive: true });
await build({
	entryPoints: [join(pkgRoot, "src", "cli.ts")],
	outfile: join(metaDir, "bin", "tabit.js"),
	bundle: true,
	platform: "node",
	format: "esm",
	target: "node24",
	external: ["@earendil-works/pi-tui"],
});
// The package page's face and the license: both ride every tarball.
cpSync(join(pkgRoot, "README.md"), join(metaDir, "README.md"));
cpSync(join(repoRoot, "LICENSE"), join(metaDir, "LICENSE"));
writeFileSync(
	join(metaDir, "package.json"),
	`${JSON.stringify(
		{
			...meta,
			main: "./bin/tabit.js",
			bin: { tabit: "./bin/tabit.js" },
			files: ["bin"],
			scripts: undefined,
			devDependencies: undefined,
			dependencies: {
				"@earendil-works/pi-tui": (meta.dependencies as Record<string, string>)["@earendil-works/pi-tui"],
			},
			optionalDependencies: Object.fromEntries(matrix.map(p => [`tabit-agent-${p}`, version])),
		},
		null,
		"\t",
	)}\n`,
);

for (const platform of toPack) {
	const core = cores.get(platform);
	if (core === undefined) {
		console.error(`pack: no core for ${platform} (pass --core ${platform}=<path>)`);
		process.exit(1);
	}
	const dash = platform.indexOf("-");
	const [os, arch] = [platform.slice(0, dash), platform.slice(dash + 1)];
	const exe = os === "win32" ? "tabit-core.exe" : "tabit-core";
	const dir = join(staging, `tabit-agent-${platform}`);
	mkdirSync(dir, { recursive: true });
	cpSync(core, join(dir, exe));
	cpSync(join(repoRoot, "LICENSE"), join(dir, "LICENSE"));
	writeFileSync(
		join(dir, "package.json"),
		`${JSON.stringify(
			{
				name: `tabit-agent-${platform}`,
				version,
				description: `the tabit-core backend binary (${platform}) for tabit-agent`,
				license: "MIT",
				os: [os],
				cpu: [arch],
				// The publish target rides the manifest (a mirror may ride dev
				// installs; uploads go only to the official registry).
				publishConfig: { registry: "https://registry.npmjs.org" },
				// Provenance (--provenance) cross-checks the attestation's repo
				// against repository.url — inherit the meta's, verbatim.
				repository: meta.repository,
			},
			null,
			"\t",
		)}\n`,
	);
}

for (const dir of [metaDir, ...[...toPack].map(p => join(staging, `tabit-agent-${p}`))]) {
	run(["npm", "pack", "--pack-destination", dist], { cwd: dir });
}

console.log(`\npacked tabit-agent@${version} + [${[...toPack].join(", ")}] into ${dist}`);

if (publish) {
	// Upload exactly the tarballs packed above (never re-tar a staging
	// dir): platform packages first, the meta last — a fresh install's
	// optional deps must resolve from the registry the moment the meta
	// becomes visible.
	for (const tgz of [...[...toPack].map(p => join(dist, `tabit-agent-${p}-${version}.tgz`)), join(dist, `tabit-agent-${version}.tgz`)]) {
		run(["npm", "publish", tgz, ...(provenance ? ["--provenance"] : [])]);
	}
	console.log(`published tabit-agent@${version} + [${[...toPack].join(", ")}]`);
} else {
	// The local install IS the published install — npm publish only uploads
	// these same tarballs. Listing the platform tarballs explicitly lets npm
	// satisfy the optional dependencies offline (they 404 on the registry
	// until published; an unpublished optional dep is skipped, and the core
	// would be silently missing). Reinstalling the same version over itself
	// is fine — iterate without version bumps.
	console.log(`install locally: npm i -g ${["tabit-agent", ...[...toPack].map(p => `tabit-agent-${p}`)].map(n => join(dist, `${n}-${version}.tgz`)).join(" ")}`);
}
