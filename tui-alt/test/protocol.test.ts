/**
 * Unit coverage for the wire vocabulary: the lenient parse's three
 * outcomes (control / event / unknown) and the serializer. The parse is
 * the TUI's only defense against a newer backend, so its rules are
 * pinned here — including the regression that once silently stripped
 * `type` out of every event.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { parseServerFrame, PROTOCOL_VERSION, toWireLine } from "../src/protocol.ts";

describe("protocol: parseServerFrame", () => {
	test("empty and whitespace-only lines are skipped (null)", () => {
		assert.strictEqual(parseServerFrame(""), null);
		assert.strictEqual(parseServerFrame("   "), null);
		assert.strictEqual(parseServerFrame("\t"), null);
	});

	test("control frames parse with their fields", () => {
		const parsed = parseServerFrame('{"type":"report","protocol_version":21}');
		assert.deepStrictEqual(parsed, {
			kind: "control",
			frame: { type: "report", protocol_version: 21 },
		});
	});

	test("stamped events keep their type and stream", () => {
		const parsed = parseServerFrame('{"type":"text_delta","stream":"019abc","turn_id":"t1","text":"hi"}');
		assert.deepStrictEqual(parsed, {
			kind: "event",
			stream: "019abc",
			origin: undefined,
			event: { type: "text_delta", turn_id: "t1", text: "hi" },
		});
	});

	test("the v18 stamps: origin is kept as attribution, ttl is stripped (consumers ignore it)", () => {
		const parsed = parseServerFrame(
			'{"type":"interaction_settled","stream":"019abc","origin":"gate-ext","ttl":7,"id":"ask1"}',
		);
		assert.deepStrictEqual(parsed, {
			kind: "event",
			stream: "019abc",
			origin: "gate-ext",
			event: { type: "interaction_settled", id: "ask1" },
		});
	});

	test("backend-level events parse without a stream stamp", () => {
		const parsed = parseServerFrame('{"type":"sessions_available","sessions":[]}');
		assert.deepStrictEqual(parsed, {
			kind: "event",
			stream: undefined,
			origin: undefined,
			event: { type: "sessions_available", sessions: [] },
		});
	});

	test("the v21 catalog parses: usable providers with per-model facts, missing_keys identities", () => {
		const parsed = parseServerFrame(
			'{"type":"models_available","providers":[{"id":"anthropic","models":[{"id":"claude","reasoning":true,"input":["text","image"],"thinking_levels":["low","high"]}]}],"missing_keys":[{"id":"openai"}]}',
		);
		assert.deepStrictEqual(parsed, {
			kind: "event",
			stream: undefined,
			origin: undefined,
			event: {
				type: "models_available",
				providers: [
					{ id: "anthropic", models: [{ id: "claude", reasoning: true, input: ["text", "image"], thinking_levels: ["low", "high"] }] },
				],
				missing_keys: [{ id: "openai" }],
			},
		});
	});

	test("session_opened.model is nullable (v21) — null parses present", () => {
		const parsed = parseServerFrame(
			'{"type":"session_opened","stream":"s1","id":"s1","path":"","cwd":"/w","model":null,"resumed":false}',
		);
		assert.deepStrictEqual(parsed, {
			kind: "event",
			stream: "s1",
			origin: undefined,
			event: { type: "session_opened", id: "s1", path: "", cwd: "/w", model: null, resumed: false },
		});
	});

	test("unknown event types are reported, never swallowed", () => {
		const parsed = parseServerFrame('{"type":"mock_heartbeat","at":123,"stream":"s"}');
		assert.strictEqual(parsed?.kind, "unknown");
		if (parsed?.kind !== "unknown") throw new Error("expected unknown");
		assert.strictEqual(typeof parsed.raw, "string");
		assert.strictEqual(parsed.type, "mock_heartbeat");
	});

	test("unparseable JSON comes back as unknown", () => {
		const parsed = parseServerFrame("{not json at all");
		assert.deepStrictEqual(parsed, { kind: "unknown", raw: "{not json at all" });
	});

	test("non-object JSON and missing type are unknown", () => {
		assert.deepStrictEqual(parseServerFrame("42"), { kind: "unknown", raw: "42" });
		assert.deepStrictEqual(parseServerFrame('"a string"'), { kind: "unknown", raw: '"a string"' });
		assert.deepStrictEqual(parseServerFrame('{"no_type":true}'), { kind: "unknown", raw: '{"no_type":true}' });
	});
});

describe("protocol: toWireLine", () => {
	test("session commands carry their session field", () => {
		const line = toWireLine({ type: "message", session: "s1", text: "hello" });
		assert.deepStrictEqual(JSON.parse(line), { type: "message", session: "s1", text: "hello" });
	});

	test("login/logout are backend-level commands (v21) — no session field", () => {
		assert.deepStrictEqual(JSON.parse(toWireLine({ type: "login", provider: "anthropic", api_key: "sk-…" })), {
			type: "login",
			provider: "anthropic",
			api_key: "sk-…",
		});
		assert.deepStrictEqual(JSON.parse(toWireLine({ type: "logout", provider: "anthropic" })), {
			type: "logout",
			provider: "anthropic",
		});
	});

	test("the declared version matches the protocol this build was written against", () => {
		// The report-model version check kills mismatches; a silent bump here
		// would strand every copy of this frontend against a backend it can't
		// talk to.
		assert.strictEqual(PROTOCOL_VERSION, 21);
	});
});
