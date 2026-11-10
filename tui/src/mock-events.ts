/**
 * The `/mock` command's canned wire events (dev-only, `--mock` sessions
 * only). One registry feeds both entry modes: the picker lists these
 * entries, and `/mock <id>` selects by name — the two can never drift.
 * Each entry carries a complete default payload so a pick is one
 * keystroke, never a text-entry chore; `event()` is a factory because
 * the timestamps must be fresh at send time.
 *
 * The mock backend re-emits the event stamped to the focused session;
 * ordering invariants are the caller's problem (this is a paintbrush,
 * not a validator). The real backend never sees the frame.
 */

export interface MockEventEntry {
	id: string;
	description: string;
	event(): Record<string, unknown>;
}

export const MOCK_EVENTS: MockEventEntry[] = [
	{
		id: "run_failed",
		description: "a run fails mid-turn (error callout)",
		event: () => ({ type: "run_failed", kind: "model", message: "provider returned 401: invalid api key", started_at_ms: Date.now(), completed_at_ms: Date.now() }),
	},
	{
		id: "run_aborted",
		description: "an abort (warn callout)",
		event: () => ({ type: "run_aborted", output: "Working on it… ", started_at_ms: Date.now(), completed_at_ms: Date.now() }),
	},
	{
		id: "turn_truncated",
		description: "the provider output limit (warn callout)",
		event: () => ({ type: "turn_truncated", turn_id: "turn-mock" }),
	},
	{
		id: "messages_discarded",
		description: "queued steering discarded (warn callout)",
		event: () => ({ type: "messages_discarded", messages: [{ id: "mock-discard", text: "also update the docs" }] }),
	},
	{
		id: "compaction_end",
		description: "the purple compaction row",
		event: () => ({ type: "compaction_end", tokens_after: 12000 }),
	},
	{
		id: "compaction_failed",
		description: "compaction fails (error callout)",
		event: () => ({ type: "compaction_failed", message: "summarizer returned an empty summary" }),
	},
	{
		id: "notice_error",
		description: "a pinned dock error",
		event: () => ({ type: "notice", id: "auth.failed", level: "error", text: "login failed: auth.toml is not writable" }),
	},
	{
		id: "notice_warn",
		description: "a transient dock warning",
		event: () => ({ type: "notice", id: "persist.degraded", level: "warn", text: "log writes degraded — 2 record(s) pending flush" }),
	},
	{
		id: "notice_info",
		description: "a transient dock info",
		event: () => ({ type: "notice", id: "startup", level: "info", text: "extension notes loaded from ~/.tabit/extensions/notes" }),
	},
	{
		id: "notice_cleared",
		description: "clear the pinned dock error",
		event: () => ({ type: "notice_cleared", id: "auth.failed" }),
	},
];

export function mockEventById(id: string): MockEventEntry | undefined {
	return MOCK_EVENTS.find(entry => entry.id === id);
}
