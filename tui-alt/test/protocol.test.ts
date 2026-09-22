/**
 * Unit coverage for the wire vocabulary: the lenient parse's three
 * outcomes (control / event / unknown) and the serializer. The parse is
 * the TUI's only defense against a newer backend, so its rules are
 * pinned here — including the regression that once silently stripped
 * `type` out of every event.
 */

import { describe, expect, test } from "bun:test";

import { parseServerFrame, PROTOCOL_VERSION, toWireLine } from "../src/protocol";

describe("protocol: parseServerFrame", () => {
	test("empty and whitespace-only lines are skipped (null)", () => {
		expect(parseServerFrame("")).toBeNull();
		expect(parseServerFrame("   ")).toBeNull();
		expect(parseServerFrame("\t")).toBeNull();
	});

	test("control frames parse with their fields", () => {
		const parsed = parseServerFrame('{"type":"initialize_ack","protocol_version":16,"session_id":"s1"}');
		expect(parsed).toEqual({
			kind: "control",
			frame: { type: "initialize_ack", protocol_version: 16, session_id: "s1" },
		});
	});

	test("stamped events keep their type and stream", () => {
		const parsed = parseServerFrame('{"type":"text_delta","stream":"019abc","turn_id":"t1","text":"hi"}');
		expect(parsed).toEqual({
			kind: "event",
			stream: "019abc",
			event: { type: "text_delta", turn_id: "t1", text: "hi" },
		});
	});

	test("backend-level events parse without a stream stamp", () => {
		const parsed = parseServerFrame('{"type":"sessions_available","sessions":[]}');
		expect(parsed).toEqual({
			kind: "event",
			stream: undefined,
			event: { type: "sessions_available", sessions: [] },
		});
	});

	test("unknown event types are reported, never swallowed", () => {
		const parsed = parseServerFrame('{"type":"mock_heartbeat","at":123,"stream":"s"}');
		expect(parsed).toEqual({ kind: "unknown", raw: expect.any(String), type: "mock_heartbeat" });
	});

	test("unparseable JSON comes back as unknown", () => {
		const parsed = parseServerFrame("{not json at all");
		expect(parsed).toEqual({ kind: "unknown", raw: "{not json at all" });
	});

	test("non-object JSON and missing type are unknown", () => {
		expect(parseServerFrame("42")).toEqual({ kind: "unknown", raw: "42" });
		expect(parseServerFrame('"a string"')).toEqual({ kind: "unknown", raw: '"a string"' });
		expect(parseServerFrame('{"no_type":true}')).toEqual({ kind: "unknown", raw: '{"no_type":true}' });
	});
});

describe("protocol: toWireLine", () => {
	test("initialize serializes with replay", () => {
		const line = toWireLine({ type: "initialize", protocol_version: PROTOCOL_VERSION, replay: true });
		expect(JSON.parse(line)).toEqual({ type: "initialize", protocol_version: 16, replay: true });
	});

	test("session commands carry their session field", () => {
		const line = toWireLine({ type: "message", session: "s1", text: "hello" });
		expect(JSON.parse(line)).toEqual({ type: "message", session: "s1", text: "hello" });
	});

	test("the declared version matches the protocol this build was written against", () => {
		// The handshake rejects mismatches; a silent bump here would strand
		// every copy of this frontend against a backend it can't talk to.
		expect(PROTOCOL_VERSION).toBe(16);
	});
});
