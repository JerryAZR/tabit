/**
 * The entry: resolve the backend command, build root + mode, run. Crash
 * handling is the report loop — exit 101 (internal error) or any
 * ungraceful death stops the TUI, restores the terminal, prints the
 * stderr tail (sessions are durable; a fresh start replays the newest
 * session, so respawn is a user re-launch in M0, a prompt in M1).
 */

import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import pkg from "../package.json";
import { Backend } from "./backend";
import { InteractiveMode } from "./mode";
import { PROTOCOL_VERSION } from "./protocol";
import { AltRoot } from "./root";
import { log } from "./log";

/**
 * Backend resolution — the zero-tuning ladder (the GUI's `backend.rs`
 * shape): `--mock[=scenario]` (dev-only) → `-c`/`--continue` → `--bin
 * <path>` → `TABIT_BIN` → the executable's own directory (the **packaged
 * sibling** — how the shipped platform package resolves) → this repo's
 * cargo outputs (dev) → `tabit` on PATH.
 *
 * Launch semantics (owner rule): no args starts a **new** session;
 * continuation is explicit via `-c`/`--continue`, which resumes the
 * project's newest session. A failed spawn is an ungraceful exit report,
 * never a hang (the spike's entry-point lesson).
 */
export function resolveBackendCommand(
	argv: string[],
	exists: (path: string) => boolean = existsSync,
	execDir: string = dirname(process.execPath),
): { bin: string; args?: string[] } {
	const mock = argv.find(a => a === "--mock" || a.startsWith("--mock="));
	if (mock !== undefined) {
		const scenario = mock.startsWith("--mock=") ? mock.slice("--mock=".length) : "basic";
		const mockPath = sourceFile("./mock-backend.ts");
		if (mockPath === undefined || !existsSync(mockPath)) {
			throw new Error("--mock is only available in a source checkout, not in a compiled build");
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
	const envBin = process.env.TABIT_BIN;
	if (envBin !== undefined && envBin !== "") return { bin: envBin, args: base };
	const exe = process.platform === "win32" ? "tabit.exe" : "tabit";
	const candidates = [
		join(execDir, exe), // the packaged sibling: the core rides next to the TUI exe
		sourceFile(`../../target-test/debug/${exe}`), // dev: the gate's build
		sourceFile(`../../target/debug/${exe}`), // dev: a plain cargo build
	];
	for (const candidate of candidates) {
		if (candidate !== undefined && exists(candidate)) return { bin: candidate, args: base };
	}
	return { bin: "tabit", args: base };
}

/**
 * Resolve a path against this source file; undefined outside a source
 * checkout (a compiled exe has no source tree behind it, and its
 * `import.meta.url` points into the bundler's virtual root).
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
	if (argv.includes("--version")) {
		process.stdout.write(`tabit-tui ${pkg.version} (protocol v${PROTOCOL_VERSION})\n`);
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
		process.stderr.write("tabit-tui needs a real terminal (TTY); run it inside a terminal emulator\n");
		process.exit(1);
	}
	log(`starting backend: ${resolved.bin} ${(resolved.args ?? ["--json", "--continue"]).join(" ")}`);

	const root = new AltRoot();
	let mode: InteractiveMode | undefined;

	const die = (message: string, code: number): never => {
		root.dispose();
		process.stderr.write(message);
		process.exit(code);
	};

	const backend = Backend.spawn(
		{ bin: resolved.bin, args: resolved.args },
		{
			onFrame: frame => mode?.handleFrame(frame),
			onExit: exit => {
				if (exit.graceful) {
					root.dispose();
					process.exit(0);
				}
				die(`the tabit backend died unexpectedly (code ${exit.code ?? "none"})\n\n${exit.stderrTail.join("\n")}\n`, 1);
			},
		},
	);

	mode = new InteractiveMode(backend, root);
	root.bind(mode, () => backend.shutdown());
	mode.onFatal = reason => die(`the backend rejected the handshake:\n\n${reason}\n`, 1);
	mode.onQuit = () => backend.shutdown();

	root.tui.start();
}

if (import.meta.main) {
	await main();
}
