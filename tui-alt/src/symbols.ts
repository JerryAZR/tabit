/**
 * Decorative unicode glyphs (⚙ ⚠ ✗ · ↳ ❯ ↓ …) are always followed by at
 * least two spaces: terminal fonts disagree about the cell width of these
 * code points, and a single space lets the following text collide with the
 * glyph. Compose marker lines through `marker` so the rule has one home —
 * typographic punctuation inside the text (em-dashes, commas) is normal
 * prose and stays single-spaced.
 */
export function marker(glyph: string, text: string): string {
	return `${glyph}  ${text}`;
}
