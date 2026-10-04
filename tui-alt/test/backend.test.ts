/**
 * The backend child seam against the mock backend *as a child process* —
 * the real spawn/pipe/exit shape. Asserts the boot ordering the protocol
 * guarantees (the report first, the boot session's announcement before
 * anything else on its stream), the fire-and-forget send path, the loud
 * spawn-failure report, and the stdin-close contractual shutdown.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { Backend } from "../src/backend.ts";
import type { ParsedServerFrame } from "../src/protocol.ts";

const MOCK = new URL("../src/mock-backend.ts", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");

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
	test("boot ordering: the report first, then the boot session's announcement, catalogs, model, replay", { timeout: 15000 }, async () => {
		const { frames, backend, exitPromise } = rigUp("replay");
		await until(() => frames.some(f => typeOf(f) === "replay_end"), 8000, "replay bracket");
		const types = frames.map(typeOf);
		assert.strictEqual(types[0], "report");
		// session_opened is the first event emission (the routing
		// guarantee: a stream is announced before any frame on its stamp).
		assert.strictEqual(types[1], "session_opened");
		assert.ok((types).includes("skills_available"));
		assert.ok((types).includes("sessions_available"));
		assert.ok((types).includes("models_available"));
		assert.ok((types).includes("model_changed"));
		assert.strictEqual(types[types.length - 1], "replay_end");
		backend.shutdown();
		const exit = await exitPromise;
		assert.strictEqual(exit.code, 0);
		assert.strictEqual(exit.graceful, true);
	});

	test("a message starts a run; outcomes arrive as events on the boot stream", { timeout: 20000 }, async () => {
		const { frames, backend, exitPromise } = rigUp("basic");
		await until(() => frames.some(f => typeOf(f) === "session_opened"), 8000, "boot announcement");
		const opened = frames.find(f => typeOf(f) === "session_opened");
		if (opened?.kind !== "event" || opened.event.type !== "session_opened") {
			throw new Error("expected the boot's session_opened");
		}
		// The routing key arrives ON the boot's stamped session_opened (v19
		// — there is no handshake ack to carry it).
		const session = opened.event.id;
		assert.strictEqual(opened.stream, session);
		backend.message(session, "say the markdown thing");
		await until(() => frames.some(f => typeOf(f) === "run_finished"), 10000, "run_finished");
		const stamped = frames.filter(f => f.kind === "event" && f.event.type === "user_message");
		assert.ok((stamped.length) > (0));
		for (const frame of stamped) {
			if (frame.kind === "event") assert.strictEqual(frame.stream, session);
		}
		backend.shutdown();
		await exitPromise;
	});

	test("login folds the world and re-announces both frames (the ack); a bad login is error { kind: auth }", { timeout: 15000 }, async () => {
		const { frames, backend, exitPromise } = rigUp("basic");
		await until(() => frames.some(f => typeOf(f) === "providers_available"), 8000, "boot catalog");
		const catalogs = () => frames.filter(f => f.kind === "event" && f.event.type === "models_available");
		const statuses = () => frames.filter(f => f.kind === "event" && f.event.type === "providers_available");

		// The boot act: one usable provider in the catalog; the status
		// frame names every configured provider's key source (v22).
		const bootCatalog = catalogs()[0]!;
		if (bootCatalog.kind !== "event" || bootCatalog.event.type !== "models_available") throw new Error("catalog");
		assert.deepStrictEqual(bootCatalog.event.providers.map(p => p.id), ["mock"]);
		const bootStatus = statuses()[0]!;
		if (bootStatus.kind !== "event" || bootStatus.event.type !== "providers_available") throw new Error("status");
		assert.deepStrictEqual(
			bootStatus.event.providers.map(p => `${p.id}:${p.auth}`),
			["locked:none", "mock:keyless"],
		);

		// login lands the key; the re-announced pair is the ack.
		backend.login("locked", "test-key");
		await until(() => catalogs().length === 2 && statuses().length === 2, 8000, "re-announced world");
		const after = catalogs()[1]!;
		if (after.kind !== "event" || after.event.type !== "models_available") throw new Error("catalog");
		assert.deepStrictEqual(after.event.providers.map(p => p.id).sort(), ["locked", "mock"]);
		const afterStatus = statuses()[1]!;
		if (afterStatus.kind !== "event" || afterStatus.event.type !== "providers_available") throw new Error("status");
		assert.deepStrictEqual(
			afterStatus.event.providers.map(p => `${p.id}:${p.auth}`),
			["locked:stored", "mock:keyless"],
		);

		// An empty key is rejected in-band: error { kind: auth }, no re-fold.
		backend.login("locked", "  ");
		await until(() => frames.some(f => f.kind === "event" && f.event.type === "error"), 8000, "auth error");
		const err = frames.find(f => f.kind === "event" && f.event.type === "error");
		if (err?.kind !== "event" || err.event.type !== "error") throw new Error("error frame");
		assert.strictEqual(err.event.kind, "auth");
		assert.strictEqual(err.stream, undefined); // backend-level

		// logout is total: unknown provider is a no-op, still acked.
		backend.logout("no-such-provider");
		await until(() => catalogs().length === 3 && statuses().length === 3, 8000, "logout ack");
		// logout of the stored provider returns it to login's target.
		backend.logout("locked");
		await until(() => statuses().length === 4, 8000, "logout fold");
		const finalStatus = statuses()[3]!;
		if (finalStatus.kind !== "event" || finalStatus.event.type !== "providers_available") throw new Error("status");
		assert.deepStrictEqual(
			finalStatus.event.providers.map(p => `${p.id}:${p.auth}`),
			["locked:none", "mock:keyless"],
		);
		backend.shutdown();
		await exitPromise;
	});

	test("spawn failure is a loud ungraceful exit, not a silent hang", { timeout: 10000 }, async () => {
		const exitPromise = new Promise<{ graceful: boolean; stderrTail: string[] }>(resolve => {
			Backend.spawn(
				{ bin: "definitely-not-a-real-tabit-binary", args: ["--json"] },
				{ onFrame: () => {}, onExit: exit => resolve(exit) },
			);
		});
		const exit = await exitPromise;
		assert.strictEqual(exit.graceful, false);
		assert.ok((exit.stderrTail.join("\n")).includes("spawn failed"));
	});
});
