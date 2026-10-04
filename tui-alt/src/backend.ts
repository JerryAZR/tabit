/**
 * The backend child process: spawn `tabit-core --json`, read its report,
 * own the pipes. The Node-port of the GUI's `backend.rs` (the reference
 * leaf-consumer): the frontend owns the process lifecycle for recovery
 * only — crash isolation is the point, and stdin close is the contractual
 * shutdown (FRONTEND.md §3.4). v19 (the report model): the backend speaks
 * first — its `report` line crosses unprompted, there is no initialize
 * handshake, and client lines are bare commands from the first line on.
 *
 * Windows specifics (TUI-RESEARCH §2/§6): `windowsHide` suppresses the
 * console flash (the GUI's CREATE_NO_WINDOW analog) and `detached` puts
 * the child in its own process group, so a console Ctrl+C — which the
 * TUI consumes as an *input* (raw mode) and maps to `abort` — never
 * reaches the backend as a death signal.
 */

import { spawn, type ChildProcess } from "node:child_process";
import * as readline from "node:readline";

import { parseServerFrame, toWireLine, type ClientFrame, type ParsedServerFrame } from "./protocol.ts";

// Lines of stderr kept for crash reports — enough for a full internal-
// error report (message plus backtrace), bounded against runaway output.
const STDERR_RING = 200;

export interface BackendOptions {
	/** Path to the `tabit-core` executable (resolution order: flag > TABIT_CORE_BIN > sibling > PATH). */
	bin: string;
	/** Extra args for the backend (default: `--json` — a new session; the
	 *  caller adds `--continue` for explicit resumption). */
	args?: string[];
	/** The project directory the backend works in (sessions live at `<cwd>/.tabit`). */
	cwd?: string;
}

export interface BackendEvents {
	/** Every parsed frame off stdout, in order. */
	onFrame: (frame: ParsedServerFrame) => void;
	/**
	 * The backend process ended. `graceful` means stdin-close shutdown;
	 * anything else is classified by exit code / EOF state by the mode.
	 */
	onExit: (exit: { code: number | null; signal: string | null; graceful: boolean; stderrTail: string[] }) => void;
}

export class Backend {
	readonly #child: ChildProcess;
	readonly #stderrRing: string[] = [];
	readonly #events: BackendEvents;
	#graceful = false;
	#exited = false;

	private constructor(child: ChildProcess, events: BackendEvents) {
		this.#child = child;
		this.#events = events;

		// Stderr → the ring, for crash reports (captured before anything
		// else takes ownership of the streams).
		readline.createInterface({ input: child.stderr! }).on("line", line => {
			this.#stderrRing.push(line);
			if (this.#stderrRing.length > STDERR_RING) this.#stderrRing.shift();
		});

		// Stdout → parsed frames. One ordered stream per connection; the
		// transport owns ordering, we never reorder (FRONTEND.md §1).
		readline.createInterface({ input: child.stdout! }).on("line", line => {
			const frame = parseServerFrame(line);
			if (frame) this.#events.onFrame(frame);
		});

		this.#child.on("exit", (code, signal) => {
			if (this.#exited) return;
			this.#exited = true;
			this.#events.onExit({
				code,
				signal,
				graceful: this.#graceful,
				stderrTail: [...this.#stderrRing],
			});
		});
		this.#child.on("error", err => {
			// Spawn failure (missing binary): a failed spawn never emits
			// 'exit', so synthesize the ungraceful exit here — a silent
			// hang is the failure mode this exists to prevent (fail loud,
			// the report carries the reason).
			this.#stderrRing.push(`spawn failed: ${err.message}`);
			if (!this.#exited) {
				this.#exited = true;
				this.#events.onExit({
					code: null,
					signal: null,
					graceful: false,
					stderrTail: [...this.#stderrRing],
				});
			}
		});
	}

	static spawn(options: BackendOptions, events: BackendEvents): Backend {
		const args = options.args ?? ["--json"];
		const child = spawn(options.bin, args, {
			cwd: options.cwd,
			stdio: ["pipe", "pipe", "pipe"],
			detached: true,
			windowsHide: true,
		});
		return new Backend(child, events);
	}

	/** One command line in. Fire-and-forget: outcomes arrive as events. */
	send(frame: ClientFrame): void {
		this.#child.stdin!.write(`${toWireLine(frame)}\n`);
	}

	message(session: string, text: string): void {
		this.send({ type: "message", session, text });
	}

	abort(session: string): void {
		this.send({ type: "abort", session });
	}

	/** Manual compaction — parks behind a running run, executes at its end. */
	compact(session: string, directives?: string): void {
		this.send({ type: "compact", session, ...(directives !== undefined ? { directives } : {}) });
	}

	/** Rewind the active chain to an entry; the backend aborts a run in
	 *  flight first and answers with `checked_out` + a full replay pass. */
	checkout(session: string, entryId: string): void {
		this.send({ type: "checkout", session, entry_id: entryId });
	}

	interactionResponse(session: string, id: string, payload: unknown): void {
		this.send({ type: "interaction_response", session, id, payload });
	}

	/** Store a provider key (v21). Backend-level, session-less; the
	 *  re-announced `models_available` is the ack, an unstamped
	 *  `error { kind: "auth" }` the failure. */
	login(provider: string, apiKey: string): void {
		this.send({ type: "login", provider, api_key: apiKey });
	}

	/** Remove a provider's key (v21). Total and idempotent — unknown
	 *  provider or absent key is a no-op, still acked by the
	 *  re-announced catalog. */
	logout(provider: string): void {
		this.send({ type: "logout", provider });
	}

	stderrTail(): string[] {
		return [...this.#stderrRing];
	}

	/**
	 * The contractual shutdown: close stdin — the core dies with the
	 * frontend (an in-flight run aborts, its terminal flushes first).
	 */
	shutdown(): void {
		this.#graceful = true;
		this.#child.stdin!.end();
	}
}
