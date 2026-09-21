/**
 * The backend child seam against the mock backend *as a child process* —
 * the real spawn/pipe/exit shape. Asserts the boot ordering the protocol
 * guarantees (ack first, boot announcement before anything on its
 * stream), the fire-and-forget send path, the loud spawn-failure report,
 * and the stdin-close contractual shutdown.
 */

import { describe, expect, test } from "bun:test";

import { Backend } from "../src/backend";
import type { ParsedServerFrame } from "../src/protocol";

const MOCK = new URL("../src/mock-backend", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");

async function until(assertion: () => boolean, timeoutMs = 8000, what = "condition"): Promise<void> {
	const deadline = Date.now() + timeoutMs;
	while (Date.now() < deadline) {
		if (assertion()) return;
		await new Promise(resolve => setTimeout(resolve, 25));
	}
	throw new Error(`timeout waiting for ${what}`);
}

function rigUp(scenario = "basic") {
	const frames: ParsedServerFrame[] = [];
	let exitResolve!: (value: { code: number | null; graceful: boolean; stderrTail: string[] }) => void;
	const exitPromise = new Promise<{ code: number | null; graceful: boolean; stderrTail: string[] }>(resolve => {
		exitResolve = resolve;
	});
	const backend = Backend.spawn(
		{ bin: process.execPath, args: [MOCK, "--scenario", scenario], cwd: process.cwd() },
		{
			onFrame: frame => frames.push(frame),
			onExit: exit => exitResolve(exit),
		},
	);
	return { frames, backend, exitPromise };
}

const typeOf = (frame: ParsedServerFrame): string | undefined => {
	if (frame.kind === "control") return frame.frame.type;
	if (frame.kind === "event") return frame.event.type;
	return frame.type;
};

describe("backend: the child seam over the mock", () => {
	test("boot ordering: ack, then the boot session's announcement, catalog, model, replay", async () => {
		const { frames, backend, exitPromise } = rigUp();
		await until(() => frames.some(f => typeOf(f) === "replay_done"), 8000, "replay bracket");
		const types = frames.map(typeOf);
		expect(types[0]).toBe("initialize_ack");
		// session_opened is the first event emission (the routing
		// guarantee: a stream is announced before any frame on its stamp).
		expect(types[1]).toBe("session_opened");
		expect(types).toContain("sessions_available");
		expect(types).toContain("model_changed");
		expect(types[types.length - 1]).toBe("replay_done");
		backend.shutdown();
		const exit = await exitPromise;
		expect(exit.code).toBe(0);
		expect(exit.graceful).toBe(true);
	}, 15000);

	test("a message starts a run; outcomes arrive as events on the boot stream", async () => {
		const { frames, backend, exitPromise } = rigUp("basic");
		await until(() => frames.some(f => typeOf(f) === "replay_done"), 8000, "handshake");
		const ack = frames[0]!;
		if (ack.kind !== "control" || ack.frame.type !== "initialize_ack") {
			throw new Error("expected initialize_ack first");
		}
		const session = ack.frame.session_id;
		backend.message(session, "say the markdown thing");
		await until(() => frames.some(f => typeOf(f) === "run_finished"), 10000, "run_finished");
		const stamped = frames.filter(f => f.kind === "event" && f.event.type === "user_message");
		expect(stamped.length).toBeGreaterThan(0);
		for (const frame of stamped) {
			if (frame.kind === "event") expect(frame.stream).toBe(session);
		}
		backend.shutdown();
		await exitPromise;
	}, 20000);

	test("spawn failure is a loud ungraceful exit, not a silent hang", async () => {
		const exitPromise = new Promise<{ graceful: boolean; stderrTail: string[] }>(resolve => {
			Backend.spawn(
				{ bin: "definitely-not-a-real-tabit-binary", args: ["--json"] },
				{ onFrame: () => {}, onExit: exit => resolve(exit) },
			);
		});
		const exit = await exitPromise;
		expect(exit.graceful).toBe(false);
		expect(exit.stderrTail.join("\n")).toContain("spawn failed");
	}, 10000);
});
