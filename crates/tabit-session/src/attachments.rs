//! Receive-time attachment expansion (owner ruling 2026-09-28): a user
//! message may carry attachment tags — `<attachment path="..."/>`, the
//! exact self-closing form — naming image files the model should see
//! (relative paths resolve against the session's working directory;
//! the frontend embeds the tag, a paste riding a temp file).
//!
//! The session expands at the message door (the mailbox, beside the
//! skill invocation expansion): tags stay in place as anchors, and each
//! resolvable tag appends a label text part and the image part, in tag
//! order — image blocks carry no metadata on any provider, so the label
//! is the correlation, and Anthropic's own guidance puts text before the
//! image. The image payload rides the shared preparation pipeline
//! (`tabit_providers::image`): sniffed by magic bytes, downscaled when
//! over the model's limits, never rejected for size alone.
//!
//! Unresolvable tags (missing file, not an image, undecodable,
//! un-fittable) stay as-is — the warning law the skill tags set:
//! `tracing::warn` to stderr, the message never rejected. The model's
//! own `read` of the path is the in-band signal: a user discussing the
//! literal tag produces no noise, and a user who meant the attachment
//! gets a model that tries the read, fails, and says so.

use std::path::{Path, PathBuf};

use tabit_providers::OneOrMany;
use tabit_providers::image::{self, Limits};
use tabit_providers::message::UserContent;

/// The attachment tag's opening text (`<attachment path="`).
const TAG_OPEN: &str = r#"<attachment path=""#;
/// The attachment tag's closing text (`"/>`).
const TAG_CLOSE: &str = r#""/>"#;

/// Expand attachment tags into content parts: the tags are scanned in
/// `scan_text` (the user's own words — a skill body documenting the tag
/// must not attach itself), while `body_text` (possibly skill-expanded)
/// is kept verbatim as the first part, tags and all — they stay as
/// anchors. Each resolvable tag then appends a label + image part, in
/// tag order. `None` when no tag resolves — the caller keeps the
/// original message untouched (the expansion is the identity for it,
/// and a plain-text message never grows parts it didn't have).
pub(crate) fn expand_attachments(
    scan_text: &str,
    body_text: &str,
    cwd: &Path,
    limits: &Limits,
) -> Option<MessageParts> {
    if !scan_text.contains(TAG_OPEN) {
        return None;
    }
    let paths = scan_attachment_tags(scan_text);
    if paths.is_empty() {
        return None;
    }
    let mut images = Vec::new();
    for path in &paths {
        match load(path, cwd, limits) {
            Ok(prepared) => images.push((path, prepared)),
            Err(reason) => {
                tracing::warn!(path = %path, %reason, "attachment tag did not resolve — left as-is");
            }
        }
    }
    if images.is_empty() {
        return None;
    }
    let total = images.len();
    let mut parts = vec![UserContent::text(body_text)];
    for (index, (path, prepared)) in images.into_iter().enumerate() {
        parts.push(UserContent::text(format!(
            "[attachment {} of {total}: {path}]",
            index + 1
        )));
        parts.push(UserContent::Image(prepared.into_image()));
    }
    Some(MessageParts { parts })
}

/// The expanded message's parts (one text part plus label/image pairs —
/// always at least two, so the `OneOrMany` construction cannot fail).
pub(crate) struct MessageParts {
    parts: Vec<UserContent>,
}

impl MessageParts {
    /// The parts as a user message's content.
    pub(crate) fn into_content(self) -> OneOrMany<UserContent> {
        #[allow(clippy::expect_used)] // non-empty by construction (see above)
        OneOrMany::many(self.parts).expect("an expansion carries at least the text part")
    }
}

/// Read and prepare one attachment: resolve against the session cwd,
/// read, run the pipeline.
fn load(path: &str, cwd: &Path, limits: &Limits) -> Result<image::Prepared, String> {
    let path = path.trim();
    let resolved: PathBuf = {
        let candidate = Path::new(path);
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            cwd.join(candidate)
        }
    };
    let bytes = std::fs::read(&resolved).map_err(|err| format!("cannot read `{path}`: {err}"))?;
    image::prepare(&bytes, limits).map_err(|rejection| format!("`{path}`: {rejection}"))
}

/// The tag paths in order of appearance — everything between
/// `<attachment path="` and `"/>`. A dangling opening (no close) stops
/// the scan; anything malformed is simply not a tag and passes through.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The one-text call shape: scan and body are the same text.
    fn expand(text: &str, cwd: &Path, limits: &Limits) -> Option<MessageParts> {
        expand_attachments(text, text, cwd, limits)
    }

    /// A minimal valid PNG (1x1), as bytes.
    fn tiny_png() -> Vec<u8> {
        let rgb = ::image::RgbImage::from_pixel(1, 1, ::image::Rgb([1, 2, 3]));
        let mut out = std::io::Cursor::new(Vec::new());
        let encoder = ::image::codecs::png::PngEncoder::new(&mut out);
        ::image::ImageEncoder::write_image(
            encoder,
            rgb.as_raw(),
            1,
            1,
            ::image::ExtendedColorType::Rgb8,
        )
        .expect("test PNG encode");
        out.into_inner()
    }

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn with_image(&self, name: &str) {
            std::fs::write(self.dir.path().join(name), tiny_png()).expect("write fixture");
        }
    }

    fn fixture() -> Fixture {
        Fixture {
            dir: tempfile::tempdir().expect("temp dir"),
        }
    }

    /// The texts of the parts, for assertions.
    fn texts(parts: &MessageParts) -> Vec<String> {
        parts
            .parts
            .iter()
            .filter_map(|part| match part {
                UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The image parts of the expansion, for assertions.
    fn images(parts: &MessageParts) -> usize {
        parts
            .parts
            .iter()
            .filter(|part| matches!(part, UserContent::Image(_)))
            .count()
    }

    #[test]
    fn a_message_without_tags_is_the_identity() {
        let cwd = fixture();
        assert!(expand("plain message", cwd.dir.path(), &Limits::default()).is_none());
        // A malformed tag is not a tag.
        assert!(
            expand(
                "look at <attachment path=\"x.png\"> please",
                cwd.dir.path(),
                &Limits::default()
            )
            .is_none()
        );
        assert!(
            expand(
                "dangling <attachment path=\"",
                cwd.dir.path(),
                &Limits::default()
            )
            .is_none()
        );
    }

    #[test]
    fn a_resolvable_tag_appends_a_labeled_image_and_keeps_its_anchor() {
        let cwd = fixture();
        cwd.with_image("shot.png");
        let parts = expand(
            "what is in <attachment path=\"shot.png\"/>?",
            cwd.dir.path(),
            &Limits::default(),
        )
        .expect("one tag resolves");
        assert_eq!(images(&parts), 1);
        let texts = texts(&parts);
        assert_eq!(texts.len(), 2);
        // The anchor stays in the message text...
        assert_eq!(texts[0], "what is in <attachment path=\"shot.png\"/>?");
        // ...and the label correlates it with the image part.
        assert_eq!(texts[1], "[attachment 1 of 1: shot.png]");
    }

    #[test]
    fn multiple_tags_append_in_tag_order() {
        let cwd = fixture();
        cwd.with_image("a.png");
        cwd.with_image("b.png");
        let parts = expand(
            "compare <attachment path=\"a.png\"/> to <attachment path=\"b.png\"/>",
            cwd.dir.path(),
            &Limits::default(),
        )
        .expect("both tags resolve");
        assert_eq!(images(&parts), 2);
        let texts = texts(&parts);
        assert_eq!(texts[1], "[attachment 1 of 2: a.png]");
        assert_eq!(texts[2], "[attachment 2 of 2: b.png]");
    }

    #[test]
    fn an_unresolvable_tag_passes_through_with_no_parts() {
        let cwd = fixture();
        // Missing file: no expansion, no parts.
        assert!(
            expand(
                "see <attachment path=\"gone.png\"/>",
                cwd.dir.path(),
                &Limits::default()
            )
            .is_none()
        );
        // A text file is not an image.
        std::fs::write(cwd.dir.path().join("notes.txt"), "hello").expect("write fixture");
        assert!(
            expand(
                "see <attachment path=\"notes.txt\"/>",
                cwd.dir.path(),
                &Limits::default()
            )
            .is_none()
        );
    }

    #[test]
    fn a_mix_resolves_what_it_can() {
        let cwd = fixture();
        cwd.with_image("here.png");
        let parts = expand(
            "<attachment path=\"gone.png\"/> and <attachment path=\"here.png\"/>",
            cwd.dir.path(),
            &Limits::default(),
        )
        .expect("one of two resolves");
        assert_eq!(images(&parts), 1);
        let texts = texts(&parts);
        // Both anchors stay; the label numbers only what resolved.
        assert!(texts[0].contains("gone.png"));
        assert_eq!(texts[1], "[attachment 1 of 1: here.png]");
    }

    #[test]
    fn absolute_paths_resolve() {
        let cwd = fixture();
        cwd.with_image("abs.png");
        let absolute = cwd.dir.path().join("abs.png");
        let tag = format!("<attachment path=\"{}\"/>", absolute.display());
        let parts = expand(&tag, Path::new("/nonexistent-cwd"), &Limits::default())
            .expect("the absolute path resolves from anywhere");
        assert_eq!(images(&parts), 1);
    }
}
