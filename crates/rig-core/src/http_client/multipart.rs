use bytes::Bytes;
use mime::Mime;
use std::borrow::Cow;

/// A generic multipart form part that can represent text or binary data
#[derive(Clone, Debug)]
pub struct Part {
    name: String,
    content: PartContent,
    filename: Option<String>,
    content_type: Option<Mime>,
}

#[derive(Clone, Debug)]
enum PartContent {
    Text(String),
    Binary(Bytes),
}

impl Part {
    /// Create a text part
    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            content: PartContent::Text(value.into()),
            filename: None,
            content_type: None,
        }
    }

    /// Create a binary part (e.g., file upload)
    pub fn bytes(name: impl Into<String>, data: impl Into<Bytes>) -> Self {
        Self {
            name: name.into(),
            content: PartContent::Binary(data.into()),
            filename: None,
            content_type: None,
        }
    }

    /// Set the filename for this part
    pub fn filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    /// Set the content type for this part
    pub fn content_type(mut self, content_type: Mime) -> Self {
        self.content_type = Some(content_type);
        self
    }

    /// Get the part name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the filename if set
    pub fn get_filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// Get the content type if set
    pub fn get_content_type(&self) -> Option<&Mime> {
        self.content_type.as_ref()
    }
}

/// Generic multipart form data container
#[derive(Clone, Debug, Default)]
pub struct MultipartForm {
    parts: Vec<Part>,
    boundary: Option<String>,
}

impl MultipartForm {
    /// Create a new empty multipart form
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a part to the form
    pub fn part(mut self, part: Part) -> Self {
        self.parts.push(part);
        self
    }

    /// Add a text field
    pub fn text(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.part(Part::text(name, value))
    }

    /// Add a file/binary field
    pub fn file(
        self,
        name: impl Into<String>,
        filename: impl Into<String>,
        content_type: Mime,
        data: impl Into<Bytes>,
    ) -> Self {
        self.part(
            Part::bytes(name, data)
                .filename(filename)
                .content_type(content_type),
        )
    }

    /// Set a custom boundary (optional, one will be generated if not set)
    pub fn boundary(mut self, boundary: impl Into<String>) -> Self {
        self.boundary = Some(boundary.into());
        self
    }

    /// Get the parts
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// Generate a boundary string
    fn generate_boundary() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("----boundary{}", timestamp)
    }

    /// Get or generate boundary
    fn get_boundary(&self) -> Cow<'_, str> {
        match &self.boundary {
            Some(b) => Cow::Borrowed(b),
            None => Cow::Owned(Self::generate_boundary()),
        }
    }

    /// Encode the multipart form to bytes with the given boundary
    pub fn encode(&self) -> (String, Bytes) {
        let boundary = self.get_boundary();
        let mut body = Vec::new();

        for part in &self.parts {
            body.extend_from_slice(b"--");
            body.extend_from_slice(boundary.as_bytes());
            body.extend_from_slice(b"\r\n");

            // Content-Disposition header
            body.extend_from_slice(b"Content-Disposition: form-data; name=\"");
            body.extend_from_slice(part.name.as_bytes());
            body.extend_from_slice(b"\"");

            if let Some(filename) = &part.filename {
                body.extend_from_slice(b"; filename=\"");
                body.extend_from_slice(filename.as_bytes());
                body.extend_from_slice(b"\"");
            }
            body.extend_from_slice(b"\r\n");

            // Content-Type header if specified
            if let Some(content_type) = &part.content_type {
                body.extend_from_slice(b"Content-Type: ");
                body.extend_from_slice(content_type.as_ref().as_bytes());
                body.extend_from_slice(b"\r\n");
            }

            body.extend_from_slice(b"\r\n");

            // Content
            match &part.content {
                PartContent::Text(text) => body.extend_from_slice(text.as_bytes()),
                PartContent::Binary(bytes) => body.extend_from_slice(bytes),
            }

            body.extend_from_slice(b"\r\n");
        }

        // Final boundary
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"--\r\n");

        (boundary.into_owned(), Bytes::from(body))
    }
}

impl From<MultipartForm> for reqwest::multipart::Form {
    fn from(value: MultipartForm) -> Self {
        let mut form = reqwest::multipart::Form::new();

        for part in value.parts {
            match part.content {
                PartContent::Text(text) => {
                    form = form.text(part.name, text);
                }
                PartContent::Binary(bytes) => {
                    let mut req_part = reqwest::multipart::Part::bytes(bytes.to_vec());
                    // A stored `Mime` round-trips through its string form, so
                    // `mime_str` cannot fail in practice (reqwest's direct
                    // `Part::mime(Mime)` constructor is private, hence the
                    // re-parse). This `From` impl has no error channel, so a
                    // failure would be an internal invariant violation and is
                    // logged loudly rather than silently swallowed.
                    if let Some(content_type) = part.content_type.as_ref() {
                        match req_part.mime_str(content_type.as_ref()) {
                            Ok(with_mime) => req_part = with_mime,
                            Err(err) => {
                                tracing::error!(
                                    content_type = %content_type,
                                    error = %err,
                                    "internal invariant violated: stored `Mime` failed to re-parse via `mime_str`; sending the part without a content type"
                                );
                                req_part = reqwest::multipart::Part::bytes(bytes.to_vec());
                            }
                        }
                    }

                    if let Some(filename) = part.filename {
                        req_part = req_part.file_name(filename);
                    }

                    form = form.part(part.name, req_part);
                }
            }
        }

        form
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multipart_encoding() {
        let form = MultipartForm::new()
            .text("field1", "value1")
            .text("field2", "value2");

        let (boundary, body) = form.encode();
        let body_str = String::from_utf8_lossy(&body);

        assert!(body_str.contains("field1"));
        assert!(body_str.contains("value1"));
        assert!(body_str.contains(&boundary));
    }

    #[test]
    fn test_file_part() {
        let form = MultipartForm::new().file(
            "upload",
            "test.txt",
            "text/plain".parse().unwrap(),
            Bytes::from("file contents"),
        );

        let (_, body) = form.encode();
        let body_str = String::from_utf8_lossy(&body);

        assert!(body_str.contains("filename=\"test.txt\""));
        assert!(body_str.contains("Content-Type: text/plain"));
        assert!(body_str.contains("file contents"));
    }

    #[test]
    fn test_part_accessors() {
        let text_part = Part::text("name", "value");
        assert_eq!(text_part.name(), "name");
        assert_eq!(text_part.get_filename(), None);
        assert_eq!(text_part.get_content_type(), None);

        let file_part = Part::bytes("data", Bytes::from_static(b"payload"))
            .filename("report.pdf")
            .content_type("application/pdf".parse().unwrap());
        assert_eq!(file_part.name(), "data");
        assert_eq!(file_part.get_filename(), Some("report.pdf"));
        let content_type = file_part.get_content_type().unwrap();
        assert_eq!(content_type.type_(), mime::APPLICATION_PDF.type_());
        assert_eq!(content_type.subtype(), mime::APPLICATION_PDF.subtype());
    }

    #[test]
    fn test_custom_boundary_is_used_in_encoding() {
        let form = MultipartForm::new()
            .boundary("custom-boundary-42")
            .text("field", "value");

        assert_eq!(form.parts().len(), 1);
        assert_eq!(form.parts()[0].name(), "field");

        let (boundary, body) = form.encode();
        assert_eq!(boundary, "custom-boundary-42");
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.starts_with("--custom-boundary-42\r\n"));
        assert!(body_str.ends_with("--custom-boundary-42--\r\n"));
    }

    #[test]
    fn test_boundary_generated_when_unset() {
        let form = MultipartForm::new().text("field", "value");
        let (boundary, body) = form.encode();
        assert!(boundary.starts_with("----boundary"));
        assert!(String::from_utf8_lossy(&body).contains(&boundary));
    }

    #[test]
    fn test_binary_part_content_is_embedded_verbatim() {
        static PAYLOAD: &[u8] = &[0, 1, 2, 255];
        let form = MultipartForm::new().part(
            Part::bytes("blob", Bytes::from_static(PAYLOAD))
                .content_type("application/octet-stream".parse().unwrap()),
        );
        let (_, body) = form.encode();
        let ct = b"Content-Type: application/octet-stream";
        assert!(body.windows(ct.len()).any(|w| w == ct));
        assert!(body.windows(PAYLOAD.len()).any(|w| w == PAYLOAD));
        assert!(body.windows(b"\r\n\r\n".len() + PAYLOAD.len()).any(|w| {
            w.ends_with(PAYLOAD) && w.starts_with(b"\r\n\r\n")
        }));
        // Binary parts without a filename omit the filename clause.
        assert!(!body.windows(b"filename".len()).any(|w| w == b"filename"));
    }

    #[test]
    fn test_convert_to_reqwest_form() {
        let form = MultipartForm::new()
            .text("field", "value")
            .file(
                "upload",
                "test.txt",
                "text/plain".parse().unwrap(),
                Bytes::from("file contents"),
            );
        // Conversion of both text and binary (mime + filename) parts must succeed.
        let converted = reqwest::multipart::Form::from(form);
        let _ = converted;

        // Empty forms convert to an empty reqwest form.
        let _ = reqwest::multipart::Form::from(MultipartForm::new());
    }
}
