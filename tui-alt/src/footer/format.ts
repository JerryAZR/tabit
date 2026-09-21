/**
 * Footer-local number formatting, shared by the badges that show token
 * volumes (context meter denominator, token legs).
 */

/** pi's proven compact counts: 421 → "421", 5300 → "5.3k", 2.4M → "2.4M". */
export function formatTokenCount(count: number): string {
	if (count < 1000) return count.toString();
	if (count < 10_000) return `${(count / 1000).toFixed(1)}k`;
	if (count < 1_000_000) return `${Math.round(count / 1000)}k`;
	if (count < 10_000_000) return `${(count / 1_000_000).toFixed(1)}M`;
	return `${Math.round(count / 1_000_000)}M`;
}
