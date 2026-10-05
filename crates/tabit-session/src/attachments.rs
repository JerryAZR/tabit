//! Receive-time attachment expansion (ROADMAP.md's attachments design
//! record, owner rulings 2026-10): a user message may carry the tag
//! `<attachment path="…"/>` — the exact self-closing form, embedded by
//! the frontend (FRONTEND.md's contract; typical UX: a paste lands as a
//! temp file, the frontend formats the tag). The session expands at the
//! message door (the mailbox's `push`, after the skill expansion): the
//! tag STAYS in the text as the anchor — the skill-tag rule — and each
//! resolvable tag appends, in tag order, a label text part (the file's
//! BASENAME — the tag already anchors the full path; a temp-file path
//! is noise to the model) and the image as a base64 content part, so
//! the model reads the message in full and correlates parts to tags by
//! name. Unresolvable tags — the file missing or unreadable, or not a
//! decodable in-scope image — pass through untouched with a warn (the
//! skill rule: the message is never rejected). Raster scope only:
//! PNG/JPEG/GIF/WebP, SNIFFED from the bytes and verified by decode —
//! the extension is never trusted.
//!
//! Downscaling happens here, once, before anything is stored or sent
//! (the faithful-copy doctrine: the durable record holds what the model
//! saw, and the server never rejects a big image): long edge ≤
//! [`MAX_LONG_EDGE`] and ≤ [`MAX_BYTES`] post-encode (a JPEG quality
//! ladder when over). An image already under both caps passes through
//! byte-identical — no gratuitous re-encode.
//!
//! Parts carry no join punctuation — the wire's `user_text` fold owns
//! the one separator (`\n\n` between text parts), so the label is the
//! bare basename and the label showing in the `user_message` event
//! text (same as skill bodies) is the ruling's intended rendering.

use std::path::Path;
use tabit_providers::completion::Message;
use tabit_providers::message::{ImageMediaType, UserContent};

/// The attachment tag's opening text (`<attachment path="`).
const TAG_OPEN: &str = r#"<attachment path=""#;
/// The attachment tag's closing text (`"/>`).
const TAG_CLOSE: &str = r#""/>"#;

/// The downscale cap: the long edge in pixels. One conservative global
/// constant (rule 1: no per-provider config, no model catalog), tunable
/// at will; 1568 matches the providers' own vision guidance.
pub(crate) const MAX_LONG_EDGE: u32 = 1568;

/// The downscale cap: post-encode bytes. 5 MB keeps under every
/// provider's per-image ceiling with headroom.
pub(crate) const MAX_BYTES: usize = 5 * 1024 * 1024;

/// The JPEG quality ladder an over-cap re-encode walks, first rung
/// first: the first encode under [`MAX_BYTES`] wins.
const JPEG_QUALITY_LADDER: [u8; 4] = [85, 70, 55, 40];

/// Expand a message's attachment tags: the message verbatim — original
/// parts first, tags intact in the text — then, per resolvable tag in
/// order of appearance, the basename label and the image part. A
/// message without tags, or whose tags all pass through, returns
/// unchanged.
#[allow(clippy::unreachable)] // sanctioned crash: the re-match's else arm is dead by the check above
pub(crate) fn expand_attachments(message: Message) -> Message {
    let Message::User { .. } = &message else {
        return message;
    };
    let text = crate::session::user_text(&message);
    if !text.contains(TAG_OPEN) {
        return message;
    }
    let mut appended: Vec<UserContent> = Vec::new();
    for path in scan_attachment_tags(&text) {
        match resolve(path) {
            Some((label, image)) => {
                appended.push(UserContent::text(label));
                appended.push(image);
            }
            None => tracing::warn!(
                path = %path,
                "attachment tag names no decodable in-scope image — left as-is"
            ),
        }
    }
    if appended.is_empty() {
        return message;
    }
    let Message::User { mut content } = message else {
        // Matched above: a user message.
        unreachable!("checked a user message above");
    };
    for part in appended {
        content.push(part);
    }
    Message::User { content }
}

/// The tag paths in order of appearance — everything between
/// `<attachment path="` and `"/>`. A dangling opening (no close) stops
/// the scan; anything malformed is simply not a tag and passes through.
/// Mirrors the skill tag's scanner (skills.rs).
fn scan_attachment_tags(text: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(TAG_OPEN) {
        let after = &rest[start + TAG_OPEN.len()..];
        let Some(end) = after.find(TAG_CLOSE) else {
            break;
        };
        paths.push(&after[..end]);
        rest = &after[end + TAG_CLOSE.len()..];
    }
    paths
}

/// Resolve one tag to its label and image part: read the file, sniff
/// the type from the bytes (never the extension), verify it decodes,
/// fit it under the caps. Any failure is external and graceful — `None`
/// passes the tag through.
fn resolve(path: &str) -> Option<(String, UserContent)> {
    let bytes = std::fs::read(path).ok()?;
    let media_type = sniff_media_type(&bytes)?;
    let decoded = image::load_from_memory(&bytes).ok()?;
    let (fitted, media_type) = fit_under_caps(decoded, bytes, media_type)?;
    // The basename is the label: the tag anchors the full path in the
    // message text. A path with no file name (a trailing `..`) labels
    // with the tag text itself — the message still reads.
    let basename = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    use base64::Engine as _;
    let image = UserContent::image_base64(
        base64::engine::general_purpose::STANDARD.encode(fitted),
        Some(media_type),
        None,
    );
    Some((basename.to_string(), image))
}

/// The in-scope raster formats, magic-byte detected (the read tool's
/// table, tabit-tools): PNG, JPEG, GIF, WebP. Anything else is not an
/// attachment this door resolves.
fn sniff_media_type(bytes: &[u8]) -> Option<ImageMediaType> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some(ImageMediaType::PNG),
        [0xff, 0xd8, 0xff, ..] => Some(ImageMediaType::JPEG),
        [b'G', b'I', b'F', b'8', b'7', b'a', ..] | [b'G', b'I', b'F', b'8', b'9', b'a', ..] => {
            Some(ImageMediaType::GIF)
        }
        // RIFF container: the bytes at 8..12 name the codec.
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some(ImageMediaType::WEBP),
        _ => None,
    }
}

/// Fit a decoded image under the caps: under both, the original bytes
/// pass through unmodified (no gratuitous re-encode); over either, a
/// thumbnail (the long edge the cap) re-encodes as JPEG down the
/// quality ladder until the bytes fit. `None` when no rung lands under
/// the byte cap — the tag passes through rather than attaching a
/// rejection-sized image.
fn fit_under_caps(
    decoded: image::DynamicImage,
    original: Vec<u8>,
    media_type: ImageMediaType,
) -> Option<(Vec<u8>, ImageMediaType)> {
    let long_edge = decoded.width().max(decoded.height());
    if long_edge <= MAX_LONG_EDGE && original.len() <= MAX_BYTES {
        return Some((original, media_type));
    }
    let scaled = if long_edge > MAX_LONG_EDGE {
        decoded.thumbnail(MAX_LONG_EDGE, MAX_LONG_EDGE)
    } else {
        decoded
    };
    // JPEG carries no alpha: flatten to RGB first (a transparent pixel
    // becomes black; the alternative — inventing a matte color — is a
    // decision, not a default).
    let rgb = scaled.to_rgb8();
    for quality in JPEG_QUALITY_LADDER {
        let mut encoded: Vec<u8> = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality);
        if encoder
            .encode(
                rgb.as_raw(),
                scaled.width(),
                scaled.height(),
                image::ExtendedColorType::Rgb8,
            )
            .is_ok()
            && encoded.len() <= MAX_BYTES
        {
            return Some((encoded, ImageMediaType::JPEG));
        }
    }
    None
}

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod tests;
