/**
 * The file sink. `console.*` is banned while the TUI owns the screen — a
 * stray stdout write corrupts the frame — so everything that would be a
 * log line (unknown frames, child-stream traffic, lifecycle notes) goes
 * here instead. Disabled unless `TABIT_TUI_LOG` names a file.
 */

import { appendFileSync } from "node:fs";

export function log(line: string): void {
	const path = process.env.TABIT_TUI_LOG;
	if (!path) return;
	try {
		appendFileSync(path, `${new Date().toISOString()} ${line}\n`);
	} catch {
		// A logging sink must never become the failure.
	}
}
