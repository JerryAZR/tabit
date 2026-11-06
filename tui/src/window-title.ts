/**
 * The terminal window title: `tabit — <cwd>`, so several tabit windows
 * tell themselves apart in the tab bar and Alt+Tab. The path folds
 * tail-first under a character budget — the discriminating information
 * lives at the tail (every project shares a `D:\Projects\…`-style head),
 * so the head is what an overflow sacrifices. The budget is in
 * characters, not components: a fixed level count both under-shows
 * `D:\a\b` and overflows on long component names.
 */

/** The path display's character budget — near where a terminal tab
 *  truncates visually anyway. */
const PATH_BUDGET = 48;

export function windowTitle(cwd: string | undefined): string {
	if (cwd === undefined || cwd === "") return "tabit";
	return `tabit — ${displayPath(cwd)}`;
}

/** The cwd as the title shows it: verbatim when it fits, otherwise the
 *  trailing components that fit behind an ellipsis. A basename that
 *  alone overflows truncates from the left, never the right — the tail
 *  is the identity. */
function displayPath(cwd: string): string {
	if (cwd.length <= PATH_BUDGET) return cwd;
	const sep = cwd.includes("\\") ? "\\" : "/";
	const components = cwd.split(/[\\/]/).filter(c => c.length > 0);
	let kept = 0;
	for (let i = components.length - 1; i >= 0; i--) {
		if (`…${sep}${components.slice(i).join(sep)}`.length > PATH_BUDGET) break;
		kept = components.length - i;
	}
	// Whole path fits behind the ellipsis only when stray separators (a
	// UNC head) account for the overflow — show it as-is.
	if (kept === components.length) return cwd;
	if (kept > 0) return `…${sep}${components.slice(-kept).join(sep)}`;
	const base = components[components.length - 1] ?? cwd;
	return `…${base.slice(-(PATH_BUDGET - 1))}`;
}
