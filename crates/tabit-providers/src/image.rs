//! Image preparation: the one pipeline from raw bytes to a provider-safe
//! image payload. Both image doors share it — the `read` tool's image arm
//! (tabit-tools) and the attachment expansion at the session's message
//! door (tabit-session) — so an oversized image gets the same treatment no
//! matter how it arrived.
//!
//! The pipeline: sniff (magic bytes — a file's extension is never
//! consulted) → measure against the model's [`Limits`] → pass through when
//! it fits, downscale-and-re-encode when it doesn't, reject only what no
//! halving can fit. Re-encodes are always JPEG (quality 85, alpha
//! composited onto white): the oversize majority is screenshots and
//! photos, which lose nothing visible. Animation collapses to the first
//! frame — documented, not hidden.

use crate::message::ImageMediaType;

/// The default largest payload, raw bytes: 3 MiB. Anthropic's per-image
/// ceiling is ~5 MiB of base64 (~3.75 MiB raw); 3 MiB keeps headroom for
/// every provider. Overridable per model via config (`image_limits`).
pub const DEFAULT_MAX_BYTES: usize = 3 * 1024 * 1024;

/// The resize floor: halving stops here. An image still over the byte
/// limit at this long edge is rejected rather than degraded into
/// unreadability.
pub const RESIZE_FLOOR_LONG_EDGE: u32 = 256;

/// The JPEG quality re-encodes use.
const JPEG_QUALITY: u8 = 85;

/// The limits one model imposes on image payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The largest accepted payload, raw bytes.
    pub max_bytes: usize,
    /// An optional long-edge pixel ceiling — token economy, not safety.
    /// Set (via config) only for models whose image pricing or handling
    /// makes full-resolution payloads wasteful. Decoding to measure is
    /// skipped entirely when this is `None` and the bytes fit.
    pub max_long_edge: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_long_edge: None,
        }
    }
}

/// Why a payload was refused. External failures (the user pointed at a
/// file), so a typed, displayable error — never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRejection {
    /// The bytes carry no known image magic.
    NotAnImage,
    /// The magic named an image but the bytes did not decode.
    Undecodable(String),
    /// Decoded, but even the resize floor could not fit the byte limit.
    TooLarge { max_bytes: usize },
}

impl std::fmt::Display for ImageRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnImage => write!(f, "not a PNG/JPEG/GIF/WebP image"),
            Self::Undecodable(err) => write!(f, "the image did not decode: {err}"),
            Self::TooLarge { max_bytes } => write!(
                f,
                "the image stayed over the {max_bytes}-byte limit even at the \
                 {RESIZE_FLOOR_LONG_EDGE}px resize floor — crop or downscale it first"
            ),
        }
    }
}

impl std::error::Error for ImageRejection {}

/// The image formats every provider carries (magic-byte detected).
/// HEIC/HEIF/SVG stay text-path files.
pub fn media_type(bytes: &[u8]) -> Option<ImageMediaType> {
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

/// A prepared payload: within the limits, ready to encode.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The payload bytes (the original when unchanged, the re-encode
    /// when resized).
    pub bytes: Vec<u8>,
    /// The payload's media type (JPEG after a resize, the sniffed type
    /// otherwise).
    pub media_type: ImageMediaType,
    /// The dimensions after preparation (the decoded size when resized;
    /// `None` for an unmeasured pass-through).
    pub dimensions: Option<(u32, u32)>,
    /// Whether the payload was downscaled from what the bytes carried in.
    pub resized: bool,
}

impl Prepared {
    /// Encode as a base64 image part (the session log and the provider
    /// wire both carry base64; raw bytes would serialize as a JSON
    /// number array).
    pub fn into_image(self) -> crate::message::Image {
        use base64::Engine as _;
        crate::message::Image {
            data: crate::message::DocumentSourceKind::Base64(
                base64::engine::general_purpose::STANDARD.encode(&self.bytes),
            ),
            media_type: Some(self.media_type),
            detail: None,
            additional_params: None,
        }
    }
}

/// Prepare raw bytes for a model: the shared sniff → measure → maybe
/// resize pipeline both image doors ride.
pub fn prepare(bytes: &[u8], limits: &Limits) -> Result<Prepared, ImageRejection> {
    let sniffed = media_type(bytes).ok_or(ImageRejection::NotAnImage)?;
    // The cheap path: fitting bytes with no long-edge limit never decode.
    if bytes.len() <= limits.max_bytes && limits.max_long_edge.is_none() {
        return Ok(Prepared {
            bytes: bytes.to_vec(),
            media_type: sniffed,
            dimensions: None,
            resized: false,
        });
    }
    let decoded = image::load_from_memory(bytes)
        .map_err(|err| ImageRejection::Undecodable(err.to_string()))?;
    if bytes.len() <= limits.max_bytes
        && limits
            .max_long_edge
            .is_none_or(|edge| decoded.width().max(decoded.height()) <= edge)
    {
        return Ok(Prepared {
            bytes: bytes.to_vec(),
            media_type: sniffed,
            dimensions: Some((decoded.width(), decoded.height())),
            resized: false,
        });
    }
    resize_to_fit(decoded, limits)
}

/// Halve (or clip to the long-edge limit) and re-encode until the
/// payload fits, stopping at the resize floor.
fn resize_to_fit(
    mut image: image::DynamicImage,
    limits: &Limits,
) -> Result<Prepared, ImageRejection> {
    loop {
        let (width, height) = (image.width(), image.height());
        let long_edge = width.max(height);
        let fits_edge = limits.max_long_edge.is_none_or(|edge| long_edge <= edge);
        let encoded = encode_jpeg(&image);
        if encoded.len() <= limits.max_bytes && fits_edge {
            return Ok(Prepared {
                bytes: encoded,
                media_type: ImageMediaType::JPEG,
                dimensions: Some((width, height)),
                resized: true,
            });
        }
        // The configured edge limit is a target, not a floor-bound step:
        // clip straight to it, even below the floor. Halving (the byte
        // limit's strategy) stops at the floor instead.
        let next_edge = match limits.max_long_edge {
            Some(edge) if long_edge > edge => edge.max(1),
            _ => {
                let halved = long_edge / 2;
                if halved < RESIZE_FLOOR_LONG_EDGE || halved >= long_edge {
                    return Err(ImageRejection::TooLarge {
                        max_bytes: limits.max_bytes,
                    });
                }
                halved
            }
        };
        image = image.resize(
            next_edge * width / long_edge,
            next_edge * height / long_edge,
            image::imageops::FilterType::Triangle,
        );
    }
}

/// Re-encode as JPEG, alpha composited onto white (JPEG carries no
/// alpha; a transparent PNG's raw RGB under transparent regions is
/// display garbage).
fn encode_jpeg(image: &image::DynamicImage) -> Vec<u8> {
    let rgba = image.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
        let [r, g, b, a] = src.0;
        let alpha = u16::from(a);
        let blend = |channel: u8| -> u8 {
            ((u16::from(channel) * alpha + 255 * (255 - alpha)) / 255) as u8
        };
        *dst = image::Rgb([blend(r), blend(g), blend(b)]);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    // The dimensions came from a successful decode, so encoding the same
    // pixels as JPEG cannot fail on format grounds; an io failure on a
    // Cursor<Vec<u8>> cannot happen either. A failure here is a bug in
    // this pipeline, not bad input.
    #[allow(clippy::expect_used)]
    image::ImageEncoder::write_image(
        encoder,
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )
    .expect("JPEG re-encode of decoded pixels is infallible");
    out.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a solid-color PNG of the given dimensions.
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let rgb = image::RgbImage::from_pixel(width, height, image::Rgb([10, 128, 200]));
        let mut out = std::io::Cursor::new(Vec::new());
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        image::ImageEncoder::write_image(
            encoder,
            rgb.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgb8,
        )
        .expect("test PNG encode");
        out.into_inner()
    }

    /// A noisy (incompressible) PNG, so the byte size tracks dimensions.
    fn noisy_png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut rgb = image::RgbImage::new(width, height);
        let mut state = 0x2545F4914F6CDD1Du64;
        for pixel in rgb.pixels_mut() {
            // xorshift64: deterministic noise.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *pixel = image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        image::ImageEncoder::write_image(
            encoder,
            rgb.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgb8,
        )
        .expect("test PNG encode");
        out.into_inner()
    }

    #[test]
    fn the_sniff_matrix() {
        assert_eq!(media_type(&png_bytes(4, 4)), Some(ImageMediaType::PNG));
        assert_eq!(
            media_type(&[0xff, 0xd8, 0xff, 0xe0]),
            Some(ImageMediaType::JPEG)
        );
        assert_eq!(media_type(b"GIF87a...."), Some(ImageMediaType::GIF));
        assert_eq!(media_type(b"GIF89a...."), Some(ImageMediaType::GIF));
        assert_eq!(media_type(b"RIFF....WEBP"), Some(ImageMediaType::WEBP));
        assert_eq!(media_type(b"plain text"), None);
        assert_eq!(media_type(b""), None);
    }

    #[test]
    fn fitting_bytes_pass_through_unmeasured() {
        let bytes = png_bytes(64, 64);
        let prepared = prepare(&bytes, &Limits::default()).expect("fits");
        assert_eq!(prepared.bytes, bytes);
        assert_eq!(prepared.media_type, ImageMediaType::PNG);
        assert!(!prepared.resized);
        assert_eq!(prepared.dimensions, None);
    }

    #[test]
    fn an_oversize_image_is_downscaled_under_the_cap() {
        let bytes = noisy_png_bytes(2000, 2000);
        assert!(bytes.len() > 1024 * 1024, "the fixture must start big");
        let limits = Limits {
            max_bytes: 1024 * 1024,
            max_long_edge: None,
        };
        let prepared = prepare(&bytes, &limits).expect("halving converges");
        assert!(prepared.bytes.len() <= limits.max_bytes);
        assert!(prepared.resized);
        assert_eq!(prepared.media_type, ImageMediaType::JPEG);
        let (w, _h) = prepared.dimensions.expect("a resize measures");
        assert!(w < 2000);
    }

    #[test]
    fn a_long_edge_limit_downscales_fitting_bytes() {
        let bytes = noisy_png_bytes(800, 400);
        let limits = Limits {
            max_bytes: usize::MAX,
            max_long_edge: Some(200),
        };
        let prepared = prepare(&bytes, &limits).expect("clips to the edge");
        assert!(prepared.resized);
        let (w, h) = prepared.dimensions.expect("a resize measures");
        assert_eq!(w.max(h), 200);
    }

    #[test]
    fn the_floor_rejects_what_cannot_fit() {
        let bytes = noisy_png_bytes(2000, 2000);
        let limits = Limits {
            max_bytes: 1024, // no image fits in a kilobyte
            max_long_edge: None,
        };
        let Err(ImageRejection::TooLarge { .. }) = prepare(&bytes, &limits) else {
            panic!("an unfittable image must be rejected, not degraded");
        };
    }

    #[test]
    fn valid_magic_with_garbage_body_is_undecodable() {
        let mut bytes = png_bytes(4, 4);
        bytes.truncate(40); // magic intact, body gone
        let limits = Limits {
            max_bytes: 1, // force the decode path
            max_long_edge: None,
        };
        let Err(ImageRejection::Undecodable(_)) = prepare(&bytes, &limits) else {
            panic!("truncated image must be undecodable");
        };
    }

    #[test]
    fn alpha_composites_onto_white() {
        // A fully transparent red pixel must come out white, not red-on-black.
        let rgba = image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 0]));
        let mut out = std::io::Cursor::new(Vec::new());
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        image::ImageEncoder::write_image(
            encoder,
            rgba.as_raw(),
            8,
            8,
            image::ExtendedColorType::Rgba8,
        )
        .expect("test PNG encode");
        let bytes = out.into_inner();
        let limits = Limits {
            max_bytes: usize::MAX,
            max_long_edge: Some(4),
        };
        let prepared = prepare(&bytes, &limits).expect("tiny image fits at 4px");
        let decoded = image::load_from_memory(&prepared.bytes).expect("the re-encode decodes");
        let pixel = decoded.to_rgb8().get_pixel(0, 0).0;
        assert_eq!(
            pixel,
            [255, 255, 255],
            "transparent red composites to white"
        );
    }
}
