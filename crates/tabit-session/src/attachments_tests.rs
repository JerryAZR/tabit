//! Attachment expansion tests: the tag, the append, the pass-throughs,
//! the downscale — the pure door function (attachments.rs). The mailbox
//! composition and the wire rendering are pinned at the endpoint level
//! (endpoint_tests.rs).

use super::*;
use std::path::PathBuf;
use tabit_providers::message::DocumentSourceKind;

/// A temp dir per test, unique per process and tag.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("tabit-attachments-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Encode a solid-color image; the test picks the format.
fn solid_png(width: u32, height: u32) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode png");
    bytes.into_inner()
}

/// A deterministic noise image (an LCG, no rand dep): noise defeats
/// PNG's compression, so a modest canvas crosses the byte cap.
fn noise_png(width: u32, height: u32) -> Vec<u8> {
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u8
    };
    let img = image::RgbImage::from_fn(width, height, |_, _| image::Rgb([next(), next(), next()]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode png");
    bytes.into_inner()
}

/// Write `bytes` to `name` under the dir, returning the path string as
/// a tag carries it.
fn plant(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("plant the file");
    path.display().to_string()
}

/// One expanded part, test-facing: its text, or its image's
/// (base64, media type).
#[derive(Debug)]
enum Part {
    Text(String),
    Image(String, Option<ImageMediaType>),
}

/// The expanded message's parts.
fn parts(message: &Message) -> Vec<Part> {
    let Message::User { content } = message else {
        panic!("a user message");
    };
    content
        .iter()
        .map(|part| match part {
            UserContent::Text(text) => Part::Text(text.text.clone()),
            UserContent::Image(image) => {
                let DocumentSourceKind::Base64(data) = &image.data else {
                    panic!("attachments ride base64");
                };
                Part::Image(data.clone(), image.media_type.clone())
            }
            other => panic!("an unexpected part: {other:?}"),
        })
        .collect()
}

/// The text of a text part.
fn text_of(part: &Part) -> Option<&str> {
    match part {
        Part::Text(text) => Some(text),
        Part::Image(..) => None,
    }
}

/// The (base64, media type) of an image part.
fn image_of(part: &Part) -> Option<(&str, &Option<ImageMediaType>)> {
    match part {
        Part::Image(data, media) => Some((data, media)),
        Part::Text(..) => None,
    }
}

fn decode_base64(data: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("valid base64")
}

#[test]
fn a_resolvable_tag_appends_label_and_image_after_the_text() {
    let dir = temp_dir("append");
    let png = solid_png(8, 6);
    let path = plant(&dir, "shot.png", &png);
    let text = format!("look <attachment path=\"{path}\"/> at this");
    let expanded = expand_attachments(Message::user(&text));

    let parts = parts(&expanded);
    assert_eq!(parts.len(), 3, "text, label, image: {parts:?}");
    assert_eq!(
        text_of(&parts[0]),
        Some(text.as_str()),
        "the text is verbatim, the tag the anchor"
    );
    assert_eq!(
        text_of(&parts[1]),
        Some("shot.png"),
        "the label is the bare basename — the wire fold owns the separator"
    );
    assert_eq!(
        crate::session::user_text(&expanded),
        format!("{text}\n\nshot.png"),
        "the joined rendering separates the label from the text"
    );
    let (data, media) = image_of(&parts[2]).expect("the image part");
    assert_eq!(media, &Some(ImageMediaType::PNG));
    assert_eq!(
        decode_base64(data),
        png,
        "an under-cap image passes through byte-identical"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multiple_tags_append_in_tag_order() {
    let dir = temp_dir("multi");
    let first = plant(&dir, "first.png", &solid_png(4, 4));
    let second = plant(&dir, "second.png", &solid_png(6, 2));
    let text = format!("<attachment path=\"{first}\"/> then <attachment path=\"{second}\"/>");
    let expanded = expand_attachments(Message::user(&text));

    let parts = parts(&expanded);
    assert_eq!(parts.len(), 5, "text, then label+image per tag");
    assert_eq!(text_of(&parts[1]), Some("first.png"));
    assert!(image_of(&parts[2]).is_some());
    assert_eq!(text_of(&parts[3]), Some("second.png"));
    assert!(image_of(&parts[4]).is_some());
    assert_eq!(
        crate::session::user_text(&expanded),
        format!("{text}\n\nfirst.png\n\nsecond.png"),
        "the fold joins the text parts with the one separator"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unresolvable_tags_pass_the_message_through_untouched() {
    let dir = temp_dir("passthrough");
    // A missing file.
    let missing = dir.join("gone.png");
    // A non-image file with an image extension: the extension is never
    // trusted.
    let fake = plant(&dir, "fake.png", b"just some text, no magic bytes");
    // Valid PNG magic over garbage: sniffed, but not a decodable image.
    let mut corrupt = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    corrupt.extend_from_slice(b"not really a png");
    let corrupt = plant(&dir, "corrupt.png", &corrupt);
    for path in [missing.display().to_string(), fake, corrupt] {
        let text = format!("see <attachment path=\"{path}\"/>");
        let expanded = expand_attachments(Message::user(&text));
        let parts = parts(&expanded);
        assert_eq!(
            parts.len(),
            1,
            "nothing appended for an unresolvable tag: {path}"
        );
        assert_eq!(text_of(&parts[0]), Some(text.as_str()), "untouched");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_image_with_a_text_extension_attaches_by_sniffing() {
    let dir = temp_dir("sniffed");
    let png = solid_png(8, 6);
    let path = plant(&dir, "actually-an-image.txt", &png);
    let expanded = expand_attachments(Message::user(format!("<attachment path=\"{path}\"/>")));
    let parts = parts(&expanded);
    assert_eq!(parts.len(), 3, "the content decides, not the name");
    assert_eq!(text_of(&parts[1]), Some("actually-an-image.txt"));
    assert_eq!(
        image_of(&parts[2]).expect("image").1,
        &Some(ImageMediaType::PNG)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_over_pixel_cap_image_is_downscaled_to_jpeg_under_both_caps() {
    let dir = temp_dir("downscale");
    let png = solid_png(3000, 2000);
    let path = plant(&dir, "big.png", &png);
    let expanded = expand_attachments(Message::user(format!("<attachment path=\"{path}\"/>")));
    let parts = parts(&expanded);
    let (data, media) = image_of(&parts[2]).expect("the image part");
    assert_eq!(media, &Some(ImageMediaType::JPEG), "re-encoded");
    let fitted = decode_base64(data);
    assert!(fitted.len() <= MAX_BYTES, "under the byte cap");
    let decoded = image::load_from_memory(&fitted).expect("the re-encode decodes");
    assert_eq!(
        (decoded.width(), decoded.height()),
        (MAX_LONG_EDGE, MAX_LONG_EDGE * 2000 / 3000),
        "the long edge is the cap, the aspect preserved"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_over_byte_cap_image_reencodes_without_a_resize() {
    let dir = temp_dir("bytecap");
    // Noise defeats PNG compression: 1400×1400 RGB noise encodes over
    // the 5 MB cap while the long edge already fits.
    let png = noise_png(1400, 1400);
    assert!(png.len() > MAX_BYTES, "the fixture crosses the byte cap");
    let path = plant(&dir, "noise.png", &png);
    let expanded = expand_attachments(Message::user(format!("<attachment path=\"{path}\"/>")));
    let parts = parts(&expanded);
    let (data, media) = image_of(&parts[2]).expect("the image part");
    assert_eq!(media, &Some(ImageMediaType::JPEG));
    let fitted = decode_base64(data);
    assert!(fitted.len() <= MAX_BYTES, "the quality ladder lands");
    let decoded = image::load_from_memory(&fitted).expect("decodes");
    assert_eq!(
        (decoded.width(), decoded.height()),
        (1400, 1400),
        "only the byte cap was over: no resize"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_self_closing_forms_and_dangling_openings_are_not_tags() {
    let dir = temp_dir("not-tags");
    let path = plant(&dir, "shot.png", &solid_png(4, 4));
    for text in [
        format!("a <attachment path=\"{path}\"> paired form is prose"),
        format!("a dangling <attachment path=\"{path}\" opening"),
        "plain text, no tag".to_string(),
    ] {
        let expanded = expand_attachments(Message::user(&text));
        assert_eq!(parts(&expanded).len(), 1, "untouched: {text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_user_messages_pass_through() {
    let assistant = Message::assistant("<attachment path=\"/x.png\"/>");
    let Message::Assistant { .. } = expand_attachments(assistant) else {
        panic!("an assistant message is never a door for attachments");
    };
}

#[test]
fn the_scan_reads_the_authored_first_part_only() {
    // The ruling pin: expansion appends parts, and an appended part
    // (a skill block, a label) mentioning the tag never attaches —
    // every door expansion scans what the user typed, so composition
    // order affects only the appended parts' ordering.
    let dir = temp_dir("first-part");
    let path = plant(&dir, "shot.png", &solid_png(4, 4));
    let skill_block =
        format!("<skill name=\"demo\">document the tag: <attachment path=\"{path}\"/></skill>");
    let message = Message::User {
        content: tabit_providers::OneOrMany::many(vec![
            UserContent::text("explain attachment tags"),
            UserContent::text(skill_block),
        ])
        .expect("two parts"),
    };
    let expanded = expand_attachments(message);
    let parts = parts(&expanded);
    assert_eq!(
        parts.len(),
        2,
        "no label/image appended for a tag outside part[0]: {parts:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
