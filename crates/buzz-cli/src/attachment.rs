//! Classification of local files offered to the relay as attachments.
//!
//! `buzz upload file` and `buzz messages send --file` share one decision: is
//! this file an image, a video, or a generic document, and what
//! `Content-Type` should the upload declare? Images and video keep the
//! media pipelines they always had. Everything else takes the relay's generic
//! file path (`crates/buzz-relay/src/api/media.rs` routes a non-image,
//! non-video body on `/upload` to `buzz_media::process_file_upload`).
//!
//! The refusals here are client-side mirrors of the relay's own, so a file the
//! relay would reject never leaves the machine.

use std::path::Path;

use crate::error::CliError;

/// Image MIME types the relay's thumbnailing pipeline accepts
/// (`ALLOWED_MIME_TYPES` in `crates/buzz-media/src/validation.rs`).
const ALLOWED_IMAGE_MIMES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// The one video MIME type the relay's video pipeline accepts.
const ALLOWED_VIDEO_MIME: &str = "video/mp4";

/// MIME types the relay refuses on the generic file path.
///
/// Mirrors `BLOCKED_FILE_MIME_TYPES` in `crates/buzz-media/src/validation.rs`.
/// `buzz-cli` does not depend on `buzz-media` (that crate pulls in the relay's
/// S3 and image stack), so the list is duplicated here and must be kept in step
/// with it. Refusing client-side means an active-content or executable file is
/// never put on the wire.
const DENIED_FILE_MIMES: &[&str] = &[
    // Active web content — stored-XSS carriers.
    "application/xhtml+xml",
    "image/svg+xml",
    "application/javascript",
    "text/javascript",
    // Native executables / installers.
    "application/x-msdownload",
    "application/x-executable",
    "application/vnd.microsoft.portable-executable",
    "application/x-mach-binary",
    "application/x-sharedlib",
    "application/x-elf",
    "application/x-msi",
    "application/vnd.android.package-archive",
    "application/x-apple-diskimage",
];

/// Extensions whose content is on [`DENIED_FILE_MIMES`] but carries no magic
/// bytes, so `infer` cannot sniff it.
///
/// SVG, JavaScript and XHTML are text; the relay's own deny check runs only on
/// a sniffed type, so these reach it as `application/octet-stream` and are
/// stored as inert downloads. Refusing them by extension here is defence in
/// depth at the point where the local file name is still known.
const DENIED_FILE_EXTENSIONS: &[(&str, &str)] = &[
    ("svg", "image/svg+xml"),
    ("svgz", "image/svg+xml"),
    ("js", "text/javascript"),
    ("mjs", "text/javascript"),
    ("cjs", "text/javascript"),
    ("xhtml", "application/xhtml+xml"),
    ("xht", "application/xhtml+xml"),
];

/// Maximum size of an image upload (50 MB) — the relay's `max_image_bytes`.
pub(crate) const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

/// Maximum size of a video upload (500 MB) — the relay's `max_video_bytes`.
pub(crate) const MAX_VIDEO_BYTES: u64 = 500 * 1024 * 1024;

/// Maximum size of a generic document upload (100 MB) — the relay's
/// `max_file_bytes` default (`crates/buzz-media/src/config.rs`).
pub(crate) const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// Largest size any upload path can accept.
///
/// Checked against the file's metadata *before* the file is read, so a file no
/// path could accept is never buffered in memory. The per-kind cap below it
/// still applies once the bytes are in hand.
pub(crate) const MAX_UPLOAD_BYTES: u64 = if MAX_VIDEO_BYTES > MAX_FILE_BYTES {
    MAX_VIDEO_BYTES
} else {
    MAX_FILE_BYTES
};

/// Maximum byte length of an attachment file name.
///
/// The name is user-sourced and is carried to every reader of the channel in
/// the event's `imeta` tag, so it is bounded before the upload runs. 255 is the
/// file-name limit of every filesystem Buzz runs on.
pub(crate) const MAX_ATTACHMENT_FILENAME_BYTES: usize = 255;

/// Which relay pipeline a file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    /// A JPEG, PNG, GIF or WebP: the thumbnailing pipeline.
    Image,
    /// An MP4: the streaming video pipeline.
    Video,
    /// Anything else the relay stores byte for byte and serves as a download.
    Document,
}

impl AttachmentKind {
    /// Largest body this pipeline accepts, in bytes.
    pub(crate) fn max_bytes(self) -> u64 {
        match self {
            Self::Image => MAX_IMAGE_BYTES,
            Self::Video => MAX_VIDEO_BYTES,
            Self::Document => MAX_FILE_BYTES,
        }
    }

    /// Whether a 404 or 405 on `/upload` may be retried on the legacy
    /// `/media/upload` endpoint.
    ///
    /// The legacy endpoint predates generic file storage and answers a
    /// non-image body with `disallowed content type`, so only the media
    /// pipelines fall back to it.
    pub(crate) fn allows_legacy_fallback(self) -> bool {
        matches!(self, Self::Image | Self::Video)
    }

    /// The markdown line `messages send` appends to the message content for an
    /// attachment of this kind.
    ///
    /// Every attachment needs one. The desktop renderer mounts an attachment
    /// card only from the anchor or image renderer, and both key on a URL
    /// literally present in the body
    /// (`desktop/src/features/messages/lib/imetaMediaMarkdown.ts`); an `imeta`
    /// tag whose URL appears nowhere in the content renders as nothing at all.
    /// Images and video get the inline `![image]`/`![video]` line they always
    /// had; a document gets the same plain link the desktop's own composer
    /// emits for a generic file, so it renders as a download card and, for a
    /// `.md`, opens in the document viewer.
    pub(crate) fn content_line(self, url: &str, filename: &str) -> String {
        match self {
            Self::Image => format!("\n![image]({url})"),
            Self::Video => format!("\n![video]({url})"),
            Self::Document => format!("\n[{}]({url})", escape_markdown_label(filename)),
        }
    }
}

/// Escape the markdown link-label metacharacters `\`, `[` and `]`.
///
/// Mirrors the desktop composer's own generic-file line, which applies
/// `label.replace(/[\\[\]]/g, "\\$&")`
/// (`desktop/src/features/messages/lib/imetaMediaMarkdown.ts`), so a file named
/// `a].pdf` still renders as a download card with the right label instead of a
/// broken link.
fn escape_markdown_label(label: &str) -> String {
    let mut escaped = String::with_capacity(label.len());
    for ch in label.chars() {
        if matches!(ch, '\\' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// A local file accepted for upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedAttachment {
    /// Which relay pipeline the file belongs to.
    pub kind: AttachmentKind,
    /// `Content-Type` the upload request declares.
    pub content_type: String,
    /// Bounded base name of the local file, for the `imeta` `filename` field.
    pub filename: String,
}

/// Lowercased extension of a file name, without the dot.
fn extension_of(filename: &str) -> Option<String> {
    Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

/// Declared `Content-Type` for a file with no magic bytes, from its extension.
///
/// Plain text formats carry no signature, so `infer` reports nothing for them
/// and the relay stores them as `application/octet-stream`. Declaring the type
/// from the extension is what lets a reader tell a report from a blob.
pub(crate) fn content_type_for_extension(filename: &str) -> &'static str {
    match extension_of(filename).as_deref() {
        Some("md" | "markdown") => "text/markdown",
        Some("html") => "text/html",
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Bounded base name of `file_path`, for the `imeta` `filename` field.
///
/// Directory components are dropped: the name travels to every reader of the
/// channel, and the local path is neither useful to them nor ours to publish.
pub(crate) fn attachment_filename(file_path: &str) -> Result<String, CliError> {
    let name = Path::new(file_path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CliError::Usage(format!("{file_path} has no usable file name")))?;
    if name.is_empty() {
        return Err(CliError::Usage(format!(
            "{file_path} has no usable file name"
        )));
    }
    if name.len() > MAX_ATTACHMENT_FILENAME_BYTES {
        return Err(CliError::Usage(format!(
            "file name too long: {} bytes (max {MAX_ATTACHMENT_FILENAME_BYTES})",
            name.len()
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(CliError::Usage(
            "file name contains a control character".to_string(),
        ));
    }
    Ok(name.to_string())
}

/// Decide which pipeline `bytes` belong to and what `Content-Type` to declare.
///
/// Runs before any request, so every refusal here keeps the file on the
/// machine.
pub(crate) fn classify_attachment(
    file_path: &str,
    bytes: &[u8],
) -> Result<ClassifiedAttachment, CliError> {
    let filename = attachment_filename(file_path)?;
    let sniffed = infer::get(bytes).map(|kind| kind.mime_type().to_string());

    let (kind, content_type) = match sniffed.as_deref() {
        Some(mime) if ALLOWED_IMAGE_MIMES.contains(&mime) => {
            (AttachmentKind::Image, mime.to_string())
        }
        Some(mime) if mime == ALLOWED_VIDEO_MIME => (AttachmentKind::Video, mime.to_string()),
        // The relay's generic path refuses every recognised media type: images
        // and video that are not the four/one the media pipelines take, and all
        // audio (no sanitizer for its containers yet).
        Some(mime)
            if mime.starts_with("image/")
                || mime.starts_with("video/")
                || mime.starts_with("audio/") =>
        {
            return Err(CliError::Usage(format!("unsupported file type: {mime}")));
        }
        Some(mime) if DENIED_FILE_MIMES.contains(&mime) => {
            return Err(CliError::Usage(format!("disallowed content type: {mime}")));
        }
        Some(mime) => (AttachmentKind::Document, mime.to_string()),
        None => {
            if let Some((_, mime)) = extension_of(&filename)
                .and_then(|ext| DENIED_FILE_EXTENSIONS.iter().find(|(e, _)| *e == ext))
            {
                return Err(CliError::Usage(format!("disallowed content type: {mime}")));
            }
            (
                AttachmentKind::Document,
                content_type_for_extension(&filename).to_string(),
            )
        }
    };

    Ok(ClassifiedAttachment {
        kind,
        content_type,
        filename,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal JPEG signature `infer` recognises.
    const JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01,
    ];
    /// Minimal PNG signature.
    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    /// Minimal MP4 `ftyp` box with the `isom` brand.
    const MP4: &[u8] = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2";
    /// PE header of a Windows executable.
    const EXE: &[u8] = b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\x00\x00\xff\xff\x00\x00";
    /// An SVG document: text, so it carries no magic bytes at all.
    const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><text>x</text></svg>";

    #[test]
    fn content_type_is_declared_from_the_extension() {
        assert_eq!(content_type_for_extension("report.md"), "text/markdown");
        assert_eq!(
            content_type_for_extension("report.markdown"),
            "text/markdown"
        );
        assert_eq!(content_type_for_extension("approval.html"), "text/html");
        assert_eq!(content_type_for_extension("spec.pdf"), "application/pdf");
        assert_eq!(content_type_for_extension("rows.csv"), "text/csv");
        assert_eq!(content_type_for_extension("data.json"), "application/json");
        assert_eq!(content_type_for_extension("notes.txt"), "text/plain");
    }

    #[test]
    fn an_unmapped_extension_falls_back_to_octet_stream() {
        assert_eq!(
            content_type_for_extension("blob.bin"),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for_extension("noextension"),
            "application/octet-stream"
        );
    }

    #[test]
    fn extension_matching_ignores_case() {
        assert_eq!(content_type_for_extension("REPORT.MD"), "text/markdown");
        assert_eq!(content_type_for_extension("Approval.HtmL"), "text/html");
        assert_eq!(content_type_for_extension("DATA.Json"), "application/json");
    }

    #[test]
    fn a_markdown_body_with_no_magic_bytes_is_accepted_as_a_document() {
        let classified = classify_attachment("/tmp/plan.md", b"# Plan\n\nbody\n").unwrap();
        assert_eq!(classified.kind, AttachmentKind::Document);
        assert_eq!(classified.content_type, "text/markdown");
        assert_eq!(classified.filename, "plan.md");
    }

    #[test]
    fn an_svg_body_is_refused_with_the_deny_list_reason() {
        let error = classify_attachment("/tmp/logo.svg", SVG).unwrap_err();
        assert_eq!(
            error.to_string(),
            "disallowed content type: image/svg+xml",
            "an SVG must be refused before any request"
        );
    }

    #[test]
    fn a_javascript_body_is_refused_with_the_deny_list_reason() {
        let error = classify_attachment("/tmp/payload.js", b"alert(1)\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "disallowed content type: text/javascript"
        );
    }

    #[test]
    fn an_executable_body_is_refused_with_the_deny_list_reason() {
        let error = classify_attachment("/tmp/tool.exe", EXE).unwrap_err();
        assert_eq!(
            error.to_string(),
            "disallowed content type: application/vnd.microsoft.portable-executable"
        );
    }

    #[test]
    fn every_denied_mime_is_refused_when_sniffed() {
        // Guard the list itself: any entry silently dropped from
        // DENIED_FILE_MIMES stops matching here.
        for mime in DENIED_FILE_MIMES {
            assert!(
                !ALLOWED_IMAGE_MIMES.contains(mime) && *mime != ALLOWED_VIDEO_MIME,
                "{mime} cannot be both allowed and denied"
            );
        }
        assert!(DENIED_FILE_MIMES.contains(&"image/svg+xml"));
        assert!(DENIED_FILE_MIMES.contains(&"text/javascript"));
    }

    #[test]
    fn images_and_video_keep_their_own_pipelines() {
        let jpeg = classify_attachment("/tmp/shot.jpg", JPEG).unwrap();
        assert_eq!(jpeg.kind, AttachmentKind::Image);
        assert_eq!(jpeg.content_type, "image/jpeg");

        let png = classify_attachment("/tmp/shot.png", PNG).unwrap();
        assert_eq!(png.kind, AttachmentKind::Image);
        assert_eq!(png.content_type, "image/png");

        let mp4 = classify_attachment("/tmp/clip.mp4", MP4).unwrap();
        assert_eq!(mp4.kind, AttachmentKind::Video);
        assert_eq!(mp4.content_type, "video/mp4");
    }

    #[test]
    fn a_sniffed_media_type_outside_the_pipelines_is_still_refused() {
        // BMP is an image `infer` recognises but neither pipeline takes, and the
        // relay's generic path refuses every `image/*`.
        let bmp = b"BM\x36\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00";
        let error = classify_attachment("/tmp/old.bmp", bmp).unwrap_err();
        assert_eq!(error.to_string(), "unsupported file type: image/bmp");
    }

    #[test]
    fn a_sniffed_audio_type_is_refused() {
        // The relay's generic path refuses every `audio/*`: it has no sanitizer
        // or location-metadata validator for those containers yet.
        let mp3 = b"ID3\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let error = classify_attachment("/tmp/clip.mp3", mp3).unwrap_err();
        assert_eq!(error.to_string(), "unsupported file type: audio/mpeg");
    }

    #[test]
    fn only_the_media_pipelines_fall_back_to_the_legacy_endpoint() {
        assert!(AttachmentKind::Image.allows_legacy_fallback());
        assert!(AttachmentKind::Video.allows_legacy_fallback());
        assert!(
            !AttachmentKind::Document.allows_legacy_fallback(),
            "the legacy endpoint refuses non-image bodies"
        );
    }

    #[test]
    fn a_document_adds_a_plain_link_line_to_the_message_content() {
        assert_eq!(
            AttachmentKind::Image.content_line("https://relay.test/a.jpg", "shot.jpg"),
            "\n![image](https://relay.test/a.jpg)"
        );
        assert_eq!(
            AttachmentKind::Video.content_line("https://relay.test/a.mp4", "clip.mp4"),
            "\n![video](https://relay.test/a.mp4)"
        );
        assert_eq!(
            AttachmentKind::Document.content_line("https://relay.test/a.bin", "plan.md"),
            "\n[plan.md](https://relay.test/a.bin)",
            "a document with no line in the body renders as nothing at all"
        );
    }

    #[test]
    fn a_document_label_escapes_markdown_metacharacters() {
        assert_eq!(
            AttachmentKind::Document.content_line("https://relay.test/a.bin", "a]b[c\\d.md"),
            "\n[a\\]b\\[c\\\\d.md](https://relay.test/a.bin)",
            "an unescaped bracket breaks the link the renderer keys on"
        );
    }

    #[test]
    fn each_kind_carries_its_own_size_cap() {
        assert_eq!(AttachmentKind::Image.max_bytes(), MAX_IMAGE_BYTES);
        assert_eq!(AttachmentKind::Video.max_bytes(), MAX_VIDEO_BYTES);
        assert_eq!(AttachmentKind::Document.max_bytes(), MAX_FILE_BYTES);
        assert!(
            MAX_UPLOAD_BYTES >= AttachmentKind::Image.max_bytes()
                && MAX_UPLOAD_BYTES >= AttachmentKind::Video.max_bytes()
                && MAX_UPLOAD_BYTES >= AttachmentKind::Document.max_bytes(),
            "the pre-read cap must not refuse a file some pipeline would accept"
        );
    }

    #[test]
    fn the_file_name_is_the_base_name_and_is_bounded() {
        assert_eq!(
            attachment_filename("/Users/a/projects/buzz/docs/plan.md").unwrap(),
            "plan.md"
        );

        let long = format!("/tmp/{}.md", "n".repeat(MAX_ATTACHMENT_FILENAME_BYTES));
        let error = attachment_filename(&long).unwrap_err();
        assert!(
            error.to_string().starts_with("file name too long:"),
            "got {error}"
        );

        let error = attachment_filename("/tmp/a\nb.md").unwrap_err();
        assert_eq!(error.to_string(), "file name contains a control character");
    }

    #[test]
    fn a_path_with_no_file_name_is_refused() {
        let error = attachment_filename("/tmp/..").unwrap_err();
        assert!(
            error.to_string().contains("no usable file name"),
            "got {error}"
        );
    }
}
