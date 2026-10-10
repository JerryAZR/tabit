#!/usr/bin/env node
/**
 * The entry: resolve the backend command, build root + mode, run. Crash
 * handling is the report loop — exit 101 (internal error) or any
 * ungraceful death stops the TUI, restores the terminal, prints the
 * stderr tail (sessions are durable; a fresh start replays the newest
 * session, so respawn is a user re-launch in M0, a prompt in M1).
 */

import { existsSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import pkg from "../package.json" with { type: "json" };
import { Backend } from "./backend.ts";
import { InteractiveMode } from "./mode.ts";
import { PROTOCOL_VERSION } from "./protocol.ts";
import { AltRoot } from "./root.ts";
import { log } from "./log.ts";

/**
 * Backend resolution — the zero-tuning ladder: `--mock[=scenario]`
 * (dev-only) → `--bin <path>` → `TABIT_CORE_BIN` → the installed
 * **platform package** (`tabit-agent-<platform>-<arch>` — the npm
 * optionalDependencies/esbuild pattern, the published app's core) → the
 * **entry's own sibling** (the release-archive layout: `tabit-core`
 * rides next to `cli.ts`) → this repo's cargo outputs (dev) →
 * `tabit-core` on PATH (a `cargo install`).
 *
 * Launch semantics (owner rule): no args starts a **new** session;
 * continuation is explicit via `-c`/`--continue`, which resumes the
 * project's newest session. A failed spawn is an ungraceful exit report,
 * never a hang (the spike's entry-point lesson).
 */
export interface ResolveDeps {
	exists?: (path: string) => boolean;
	/** Resolve an installed platform package's directory (undefined when
	 *  not installed) — injectable; the real one rides import.meta. */
	resolvePackage?: (name: string) => string | undefined;
}

export function resolveBackendCommand(argv: string[], deps: ResolveDeps = {}): { bin: string; args?: string[] } {
	const exists = deps.exists ?? existsSync;
	const resolvePackage = deps.resolvePackage ?? defaultResolvePackage;
	const mock = argv.find(a => a === "--mock" || a.startsWith("--mock="));
	if (mock !== undefined) {
		const scenario = mock.startsWith("--mock=") ? mock.slice("--mock=".length) : "basic";
		const mockPath = sourceFile("./mock-backend.ts");
		if (mockPath === undefined || !existsSync(mockPath)) {
			throw new Error("--mock is only available in a source checkout (the published package excludes the mock)");
		}
		return { bin: process.execPath, args: [mockPath, "--scenario", scenario] };
	}
	const continues = argv.includes("-c") || argv.includes("--continue");
	const base = continues ? ["--json", "--continue"] : ["--json"];
	const binFlag = argv.indexOf("--bin");
	if (binFlag !== -1) {
		const value = argv[binFlag + 1];
		if (value !== undefined) return { bin: value, args: base };
	}
	const envBin = process.env.TABIT_CORE_BIN;
	if (envBin !== undefined && envBin !== "") return { bin: envBin, args: base };
	const exe = process.platform === "win32" ? "tabit-core.exe" : "tabit-core";
	// The published app: the lockstepped platform package carries the core.
	const pkgDir = resolvePackage(`tabit-agent-${process.platform}-${process.arch}`);
	if (pkgDir !== undefined) {
		const candidate = join(pkgDir, exe);
		if (exists(candidate)) return { bin: candidate, args: base };
	}
	const candidates = [
		sourceFile(`./${exe}`), // the release archive: the core next to this entry
		sourceFile(`../../target-test/debug/${exe}`), // dev: the gate's build
		sourceFile(`../../target/debug/${exe}`), // dev: a plain cargo build
	];
	for (const candidate of candidates) {
		if (candidate !== undefined && exists(candidate)) return { bin: candidate, args: base };
	}
	return { bin: "tabit-core", args: base };
}

/** The npm rung's resolver: an installed platform package's directory,
 *  found through node resolution from this module (hoisted or nested —
 *  both resolve). Undefined when the package isn't installed. */
function defaultResolvePackage(name: string): string | undefined {
	try {
		return dirname(fileURLToPath(import.meta.resolve(`${name}/package.json`)));
	} catch {
		return undefined;
	}
}

/**
 * The `--debug` switch: the backend's TRACE log and the TUI's own log,
 * side by side next to the session logs — the backend child gets
 * `TABIT_DEBUG_LOG=1` (its default is `<cwd>/.tabit/sessions/debug.log`),
 * the TUI log defaults to `tui.log` in the same directory. Explicit env
 * wins: a preset `TABIT_DEBUG_LOG`/`TABIT_TUI_LOG` is never clobbered.
 * Returns the env addition for the backend child.
 */
export function applyDebugFlag(argv: string[], cwd: string = process.cwd()): Record<string, string> | undefined {
	if (!argv.includes("--debug")) return undefined;
	if (!process.env.TABIT_TUI_LOG) {
		const tuiLog = join(cwd, ".tabit", "sessions", "tui.log");
		try {
			mkdirSync(dirname(tuiLog), { recursive: true });
			process.env.TABIT_TUI_LOG = tuiLog;
			process.stderr.write(`tabit: TUI log at ${tuiLog}\n`);
		} catch (error) {
			process.stderr.write(`tabit: cannot create the TUI log directory: ${error instanceof Error ? error.message : String(error)}\n`);
		}
	}
	if (!process.env.TABIT_DEBUG_LOG) return { TABIT_DEBUG_LOG: "1" };
	return undefined;
}

/**
 * Resolve a path against this source file; undefined when the module URL
 *  isn't a file (a bundled/embedded runtime has no source tree behind it).
 */
function sourceFile(rel: string): string | undefined {
	try {
		return fileURLToPath(new URL(rel, import.meta.url));
	} catch {
		return undefined;
	}
}

async function main(): Promise<void> {
	const argv = process.argv.slice(2);
	if (argv.includes("--help")) {
		process.stdout.write(
			"usage: tabit [-c|--continue] [--bin <path>] [--version] [--print-backend]\n" +
				"\n" +
				"  no args            start a new session\n" +
				"  -c, --continue     resume the project's newest session\n" +
				"  --bin <path>       use this tabit-core binary\n" +
				"  --debug            write debug logs (backend trace + TUI log) to .tabit/sessions/\n" +
				"  --version          print the version and protocol\n" +
				"  --print-backend    print the resolved backend command and exit\n" +
				"\n" +
				"docs: https://github.com/JerryAZR/tabit\n",
		);
		return;
	}
	if (argv.includes("--version")) {
		process.stdout.write(`tabit-agent ${pkg.version} (protocol v${PROTOCOL_VERSION})\n`);
		return;
	}

	let resolved: { bin: string; args?: string[] };
	try {
		resolved = resolveBackendCommand(argv);
	} catch (error) {
		process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
		process.exit(1);
	}
	if (argv.includes("--print-backend")) {
		// Headless diagnostic: which backend would this launch, and why that
		// one (the ladder is unit-tested; this proves the installed artifact).
		process.stdout.write(`${JSON.stringify(resolved)}\n`);
		return;
	}
	if (process.stdout.isTTY !== true) {
		// The engine takes the terminal raw; running headless would hang on
		// its own stdin. Fail loud, never hang (the spike's entry rule).
		process.stderr.write("tabit needs a real terminal (TTY); run it inside a terminal emulator\n");
		process.exit(1);
	}
	log(`starting backend: ${resolved.bin} ${(resolved.args ?? ["--json", "--continue"]).join(" ")}`);
	const debugEnv = applyDebugFlag(argv);

	const root = new AltRoot();
	let mode: InteractiveMode | undefined;

	const die = (message: string, code: number): never => {
		root.dispose();
		process.stderr.write(message);
		process.exit(code);
	};

	const backend = Backend.spawn(
		{ bin: resolved.bin, args: resolved.args, ...(debugEnv !== undefined ? { env: debugEnv } : {}) },
		{
			onFrame: frame => mode?.handleFrame(frame),
			onExit: exit => {
				if (exit.graceful) {
					root.dispose();
					process.exit(0);
				}
				// v19: a backend-internal wind-down resolves the connection —
				// stdout EOF and a clean exit, no error. Treat it as the
				// disconnect it is, not a crash (FRONTEND.md §3).
				if (exit.code === 0) {
					root.dispose();
					process.stderr.write("the backend ended the connection\n");
					process.exit(0);
				}
				die(`the tabit backend died unexpectedly (code ${exit.code ?? "none"})\n\n${exit.stderrTail.join("\n")}\n`, 1);
			},
		},
	);

	mode = new InteractiveMode(backend, root);
	root.bind(mode, () => backend.shutdown());
	mode.onFatal = reason => die(`${reason}\n`, 1);
	mode.onQuit = () => backend.shutdown();

	root.tui.start();
}

if (import.meta.main) {
	await main();
}
