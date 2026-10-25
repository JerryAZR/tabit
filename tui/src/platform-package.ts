/**
 * The platform package's npm name — the ONE place the name is derived
 * (pack.ts packs and publishes by it, the runtime resolves by it, so the
 * two can never drift).
 *
 * `win32` spells out to `windows` in the name: npm's spam filter blocked
 * `tabit-agent-win32-x64` at first publish (2026-10 — name-level 403,
 * support route uncertain), so the Windows packages publish under the
 * spelled-out os. The `os`/`cpu` manifest gates keep the real platform
 * tokens; only the name is affected.
 */

/** The npm name for a combined platform token (`"win32-x64"`, as in
 *  pack.ts's matrix and `--core` keys). */
export function platformPackageName(platform: string): string {
	return `tabit-agent-${platform.replace(/^win32-/, "windows-")}`;
}

/** This host's platform token (`"${process.platform}-${process.arch}"`). */
export function hostPlatform(): string {
	return `${process.platform}-${process.arch}`;
}
