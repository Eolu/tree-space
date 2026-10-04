//! Inline file previews.
//!
//! "View Thumbnail" is really a generic inline-preview toggle: it shows a small
//! rendering of a file below its row. This module decides *what* a file can
//! show and loads the data; [`crate::ui::tree`] turns that data into widgets.
//!
//! Detection is content-based. The file's first bytes are handed to GIO's
//! `content_type_guess` alongside its name, so extensionless and misnamed files
//! classify correctly (`fake.png` holding text is text, not an image). A
//! well-known extension is only consulted as a tiebreaker when the content is
//! unrecognisably generic (an empty file, say).
//!
//! [`PreviewKind`] is the single registry: add a variant there, teach
//! [`kind_for_content_type`] about it, add a [`DocumentData`] arm, and the tree
//! gains the new preview.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use relm4::gtk::gio;

use crate::highlight::{self, Language, Span};

/// What an inline preview can render for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    /// A still image, decoded to a `GdkTexture`.
    Image,
    /// An animated GIF: first frame still, animates on click.
    Gif,
    /// A video: first frame (paused) still, plays on click.
    Video,
    /// An audio file: no picture, just an inline transport.
    Audio,
    /// Plain text, source code, logs and the like.
    Text,
    /// A comma- or tab-separated table.
    Csv,
    /// A JSON document.
    Json,
    /// A TOML document.
    Toml,
    /// A YAML document.
    Yaml,
    /// An archive (or compressed single file).
    Archive,
}

impl PreviewKind {
    /// Whether this kind is rendered by a media widget rather than the
    /// document renderer.
    pub fn is_media(self) -> bool {
        matches!(self, Self::Image | Self::Gif | Self::Video | Self::Audio)
    }

    /// The menu label for the preview toggle, when it should read differently
    /// from the configured "View Thumbnail" (there is no picture to view).
    pub fn menu_label(self) -> Option<&'static str> {
        match self {
            Self::Audio => Some("Show Player"),
            Self::Video => Some("Show Video"),
            Self::Text | Self::Csv | Self::Json | Self::Toml | Self::Yaml => Some("Show Preview"),
            Self::Archive => Some("Show Contents"),
            Self::Image | Self::Gif => None,
        }
    }
}

/// Bytes handed to GIO for content sniffing.
const SNIFF_BYTES: u64 = 8192;
/// How much of a document is read at all.
const PREVIEW_BYTES: u64 = 64 * 1024;
/// Lines kept for a text or structured preview. The preview is scrollable; this
/// cap (with [`PREVIEW_BYTES`]) keeps a rebuild cheap while still showing far
/// more than the panel height.
const PREVIEW_LINES: usize = 300;
/// Lines shown for a CSV/TSV preview (the first is treated as a header).
const TABLE_ROWS: usize = 8;
/// Columns shown for a CSV/TSV preview.
const TABLE_COLS: usize = 6;
/// Longest cell rendered in a table, in characters.
const MAX_CELL_CHARS: usize = 32;
/// Archive entries listed before summarising the rest.
const ARCHIVE_ENTRIES: usize = 12;

/// The preview `path` supports, if any. Directories return `None`: the caller
/// previews their children instead.
pub fn detect(path: &Path) -> Option<PreviewKind> {
    if !path.is_file() {
        return None;
    }
    match raw_content_type(path) {
        Some(ctype) => kind_for_content_type(&ctype).or_else(|| extension_kind(path)),
        None => extension_kind(path),
    }
}

/// The content type for `path`, content-first.
///
/// GIO guesses from the name *and* the bytes together, but lets a known
/// extension win even when the bytes disagree (`fake.png` holding text still
/// guesses `image/png`). So we guess twice: once from the bytes alone
/// (`data_only`) and once from name+bytes (`both`). A distinctive binary
/// signature from the bytes wins outright; a plain-text body refines to the
/// name's text subtype (JSON, CSV, ...) but is never overridden into a binary
/// type; and a generic body falls back to the name. `None` means "nothing
/// recognisable".
fn raw_content_type(path: &Path) -> Option<String> {
    let data = read_prefix(path, SNIFF_BYTES).unwrap_or_default();
    let (data_only, _) = gio::content_type_guess(None::<&Path>, Some(&data[..]));
    let (both, _) = gio::content_type_guess(Some(path), Some(&data[..]));
    let data_only = data_only.to_string();
    let both = both.to_string();

    if is_generic(&data_only) {
        return (!is_generic(&both)).then_some(both);
    }
    if is_textual(&data_only) {
        // The body is text. Keep a text subtype the name suggests, but never
        // accept a binary type just because the extension claims one.
        return Some(if is_textual(&both) { both } else { data_only });
    }
    Some(data_only)
}

/// Types that carry no information (an unknown or empty body).
fn is_generic(ctype: &str) -> bool {
    ctype.is_empty() || ctype == "application/octet-stream" || ctype == "application/x-zerosize"
}

/// Map a content type onto a preview kind, or `None` when we do not preview it.
fn kind_for_content_type(ctype: &str) -> Option<PreviewKind> {
    // SVG is text, and GdkTexture cannot rasterise it, so show the source.
    if ctype == "image/svg+xml" {
        return Some(PreviewKind::Text);
    }
    if ctype == "image/gif" {
        return Some(PreviewKind::Gif);
    }
    if gio::functions::content_type_is_a(ctype, "image/*") {
        return Some(PreviewKind::Image);
    }
    if gio::functions::content_type_is_a(ctype, "video/*") {
        return Some(PreviewKind::Video);
    }
    if gio::functions::content_type_is_a(ctype, "audio/*") {
        return cfg!(feature = "audio").then_some(PreviewKind::Audio);
    }
    // Ogg is a container that is usually audio (Vorbis); give it the player.
    if cfg!(feature = "audio")
        && matches!(ctype, "application/ogg" | "application/x-ogg")
    {
        return Some(PreviewKind::Audio);
    }
    if is_archive(ctype) {
        return Some(PreviewKind::Archive);
    }
    if is_csv(ctype) {
        return Some(PreviewKind::Csv);
    }
    if let Some(kind) = structured_kind(ctype) {
        return Some(kind);
    }
    if is_text(ctype) {
        return Some(PreviewKind::Text);
    }
    None
}

/// CSV/TSV content types.
fn is_csv(ctype: &str) -> bool {
    matches!(
        ctype,
        "text/csv" | "text/tab-separated-values" | "application/csv" | "text/x-csv"
            | "text/x-comma-separated-values"
    )
}

/// The concrete structured format a content type names, if any.
fn structured_kind(ctype: &str) -> Option<PreviewKind> {
    match ctype {
        "application/json" | "text/json" | "application/x-json" | "application/ld+json"
        | "application/x-ndjson" => Some(PreviewKind::Json),
        "application/toml" | "text/toml" | "text/x-toml" | "application/x-toml" => {
            Some(PreviewKind::Toml)
        }
        "application/yaml" | "text/yaml" | "text/x-yaml" | "application/x-yaml"
        | "application/x-yml" | "text/x-yml" => Some(PreviewKind::Yaml),
        _ => None,
    }
}

/// Whether `ctype` describes text we can show (plain, structured or tabular).
fn is_textual(ctype: &str) -> bool {
    is_text(ctype) || is_csv(ctype) || structured_kind(ctype).is_some()
}

/// Whether `ctype` is some kind of human-readable text (used for the generic
/// text preview, beyond the `text/*` supertype).
fn is_text(ctype: &str) -> bool {
    gio::functions::content_type_is_a(ctype, "text/*")
        || matches!(
            ctype,
            "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/x-shellscript"
                | "application/x-perl"
                | "application/x-python"
                | "application/x-ruby"
                | "application/x-tcl"
                | "application/x-awk"
                | "application/x-php"
                | "application/x-httpd-php"
                | "application/x-lua"
                | "application/x-haskell"
                | "application/x-tex"
                | "application/x-texinfo"
                | "application/x-desktop"
                | "application/x-wine-extension-ini"
        )
}

/// Archive and compressed-container types we at least label (not all are
/// listable; see [`list_archive`]).
fn is_archive(ctype: &str) -> bool {
    matches!(
        ctype,
        "application/zip"
            | "application/x-tar"
            | "application/x-gtar"
            | "application/gzip"
            | "application/x-gzip"
            | "application/x-compressed-tar"
            | "application/x-bzip2"
            | "application/x-bzip"
            | "application/x-bzip-compressed-tar"
            | "application/x-xz"
            | "application/x-xz-compressed-tar"
            | "application/x-lzma"
            | "application/x-lz4"
            | "application/x-lzip"
            | "application/zstd"
            | "application/x-zstd"
            | "application/x-7z-compressed"
            | "application/vnd.rar"
            | "application/x-rar"
            | "application/x-rar-compressed"
            | "application/x-cpio"
            | "application/x-archive"
            | "application/x-arj"
            | "application/x-iso9660-image"
            | "application/x-cd-image"
            | "application/x-apple-diskimage"
            | "application/vnd.debian.binary-package"
            | "application/x-deb"
            | "application/x-rpm"
            | "application/java-archive"
            | "application/epub+zip"
            | "application/vnd.android.package-archive"
            | "application/x-compress"
    )
}

/// The preview kind implied by a well-known extension. Only a fallback for when
/// the content is generic; content sniffing wins whenever it recognises the
/// file.
fn extension_kind(path: &Path) -> Option<PreviewKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if cfg!(feature = "audio")
        && matches!(
            ext.as_str(),
            "mp3" | "m4a" | "m4b" | "aac" | "flac" | "wav" | "opus" | "oga" | "wma" | "aif"
                | "aiff" | "alac"
        )
    {
        return Some(PreviewKind::Audio);
    }
    Some(match ext.as_str() {
        "gif" => PreviewKind::Gif,
        "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tif" | "tiff" | "avif" | "heic" | "heif" => {
            PreviewKind::Image
        }
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "flv" | "ogv" | "ogg" | "mpg"
        | "mpeg" | "3gp" | "m2ts" => PreviewKind::Video,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "lz4" | "lz"
        | "iso" | "deb" | "rpm" | "jar" | "epub" | "apk" | "cpio" | "ar" => PreviewKind::Archive,
        "csv" | "tsv" => PreviewKind::Csv,
        "json" => PreviewKind::Json,
        "toml" => PreviewKind::Toml,
        "yaml" | "yml" => PreviewKind::Yaml,
        "txt" | "md" | "markdown" | "log" | "rs" | "go" | "c" | "h" | "cpp" | "hpp" | "py" | "js"
        | "ts" | "sh" | "ini" | "conf" | "cfg" | "xml" | "html" | "css" | "svg" => {
            PreviewKind::Text
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// document loading
// ---------------------------------------------------------------------------

/// A parsed inline document, ready to render. One variant per non-media
/// [`PreviewKind`], so the tree's render match stays exhaustive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentData {
    /// Plain text (and the raw source of any non-table document).
    Lines(DocumentLines),
    /// A CSV/TSV table.
    Table(TableData),
    /// A structured file's raw lines plus its parse result.
    Structured {
        lines: DocumentLines,
        status: ParseStatus,
    },
    /// An archive's table of contents, or a summary when it can't be listed.
    Archive(ArchiveData),
}

/// Load the document behind `kind`. `None` when the file can't be read or the
/// kind is media (media is decoded by the tree, not here).
pub fn load_document(path: &Path, kind: PreviewKind) -> Option<DocumentData> {
    let language = highlight::language_for(path);
    match kind {
        PreviewKind::Text => {
            let text = read_text(path, PREVIEW_BYTES)?;
            Some(DocumentData::Lines(lines_from(&text, PREVIEW_LINES, language)))
        }
        PreviewKind::Json | PreviewKind::Toml | PreviewKind::Yaml => {
            let text = read_text(path, PREVIEW_BYTES)?;
            Some(DocumentData::Structured {
                lines: lines_from(&text, PREVIEW_LINES, language),
                status: parse_status(&text, kind),
            })
        }
        PreviewKind::Csv => read_table(path, TABLE_ROWS, TABLE_COLS).map(DocumentData::Table),
        PreviewKind::Archive => Some(DocumentData::Archive(list_archive(path))),
        PreviewKind::Image | PreviewKind::Gif | PreviewKind::Video | PreviewKind::Audio => None,
    }
}

/// The first `max_lines` lines of `text`, each capped in length, plus whether
/// the file continues past what is shown and the syntax spans over the joined
/// text (empty for a language that isn't highlighted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentLines {
    pub lines: Vec<String>,
    pub more: bool,
    /// Highlight spans over `lines.join("\n")`, in character offsets.
    pub syntax: Vec<Span>,
}

/// A parsed table: up to `max_rows` rows of up to `max_cols` cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableData {
    pub rows: Vec<Vec<String>>,
    pub more: bool,
}

/// The result of parsing a structured document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseStatus {
    pub ok: bool,
    pub detail: String,
}

/// An archive's listing, or a description when its contents can't be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveData {
    Listed(ArchiveListing),
    Unsupported(String),
}

/// The first [`ARCHIVE_ENTRIES`] entries of an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveListing {
    pub entries: Vec<ArchiveEntry>,
    pub more: bool,
}

/// One archive member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

fn read_prefix(path: &Path, max: u64) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn read_text(path: &Path, max: u64) -> Option<String> {
    let bytes = read_prefix(path, max)?;
    Some(decode_text(&bytes))
}

/// Decode preview bytes as text: honour a UTF-16 BOM (common in Windows
/// `desktop.ini` files), otherwise treat the bytes as UTF-8. NUL bytes are
/// always dropped — GTK string APIs panic on interior NULs, so a binary or
/// UTF-16 file opened as text must never take the panel down.
fn decode_text(bytes: &[u8]) -> String {
    let decoded = if bytes.starts_with(&[0xFF, 0xFE]) {
        utf16_to_string(&bytes[2..], true)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        utf16_to_string(&bytes[2..], false)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    decoded.replace('\0', "")
}

/// Decode UTF-16 code units (the byte pairs after a BOM) into a string.
fn utf16_to_string(bytes: &[u8], little_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Split `text` into at most `max_lines` display lines, highlighting the result
/// as `language`. The spans index the joined text the tree renders.
fn lines_from(text: &str, max_lines: usize, language: Language) -> DocumentLines {
    let mut all = text.lines();
    let lines: Vec<String> = all.by_ref().take(max_lines).map(truncate_line).collect();
    // `all` is not exhausted if the file continued past the cap.
    let more = all.next().is_some();
    let syntax = highlight::highlight(language, &lines.join("\n"));
    DocumentLines { lines, more, syntax }
}

/// Cap a line so a minified file can't produce an unbounded label.
fn truncate_line(line: &str) -> String {
    const MAX_LINE_CHARS: usize = 400;
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(MAX_LINE_CHARS).collect();
    out.push('…');
    out
}

/// Parse `text` as `kind` and describe the outcome. Every parser's message
/// already names the line and column, so it is shown as-is.
fn parse_status(text: &str, kind: PreviewKind) -> ParseStatus {
    match kind {
        PreviewKind::Json => {
            status_of(serde_json::from_str::<serde_json::Value>(text).err(), "valid JSON")
        }
        PreviewKind::Toml => status_of(toml::from_str::<toml::Value>(text).err(), "valid TOML"),
        PreviewKind::Yaml => {
            status_of(serde_norway::from_str::<serde_norway::Value>(text).err(), "valid YAML")
        }
        _ => ParseStatus { ok: true, detail: String::new() },
    }
}

/// Turn an optional parse error into a status line.
fn status_of(error: Option<impl std::fmt::Display>, valid: &str) -> ParseStatus {
    match error {
        None => ParseStatus { ok: true, detail: valid.to_owned() },
        Some(err) => ParseStatus {
            ok: false,
            detail: first_line(&err.to_string()).to_owned(),
        },
    }
}

/// The first line of a (possibly multi-line) error message, trimmed.
fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or(message).trim()
}

fn read_table(path: &Path, max_rows: usize, max_cols: usize) -> Option<TableData> {
    let text = read_text(path, PREVIEW_BYTES)?;
    // Sniff the delimiter from the first line rather than trusting the
    // extension: a `.csv` exported with tabs should still line up.
    let delimiter = first_line_of(&text)
        .map(|line| {
            if line.matches('\t').count() > line.matches(',').count() {
                '\t'
            } else {
                ','
            }
        })
        .unwrap_or(',');

    let mut lines = text.lines();
    let rows: Vec<Vec<String>> = lines
        .by_ref()
        .take(max_rows)
        .map(|line| {
            split_fields(line, delimiter)
                .into_iter()
                .take(max_cols)
                .map(|cell| truncate_cell(&cell))
                .collect()
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    let more = lines.next().is_some();
    Some(TableData { rows, more })
}

fn first_line_of(text: &str) -> Option<&str> {
    text.lines().find(|line| !line.trim().is_empty())
}

/// Split one CSV/TSV line, honouring double-quoted fields (`""` is a literal
/// quote). Deliberately forgiving: a preview does not need full RFC 4180.
fn split_fields(line: &str, delimiter: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == delimiter && !quoted => fields.push(std::mem::take(&mut field)),
            c => field.push(c),
        }
    }
    fields.push(field);
    fields
}

fn truncate_cell(cell: &str) -> String {
    let cell = cell.trim();
    if cell.chars().count() <= MAX_CELL_CHARS {
        return cell.to_owned();
    }
    let mut out: String = cell.chars().take(MAX_CELL_CHARS).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// archives
// ---------------------------------------------------------------------------

/// Which listing strategy an archive needs.
enum ArchiveFlavor {
    Zip,
    /// A gzip stream wrapping a tar.
    GzipTar,
    Tar,
    /// A container we recognise but do not list.
    Other(&'static str),
}

fn list_archive(path: &Path) -> ArchiveData {
    match archive_flavor(path) {
        ArchiveFlavor::Zip => list_zip(path),
        ArchiveFlavor::GzipTar => list_tar(path, true),
        ArchiveFlavor::Tar => list_tar(path, false),
        ArchiveFlavor::Other(name) => {
            ArchiveData::Unsupported(format!("{name} archive, contents not listed"))
        }
    }
}

fn archive_flavor(path: &Path) -> ArchiveFlavor {
    if let Some(ctype) = raw_content_type(path) {
        match ctype.as_str() {
            "application/zip" => return ArchiveFlavor::Zip,
            "application/gzip" | "application/x-gzip" | "application/x-compressed-tar" => {
                return ArchiveFlavor::GzipTar;
            }
            "application/x-tar" | "application/x-gtar" => return ArchiveFlavor::Tar,
            _ => {}
        }
    }
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("zip" | "jar" | "apk" | "epub") => ArchiveFlavor::Zip,
        Some("gz" | "tgz") => ArchiveFlavor::GzipTar,
        Some("tar") => ArchiveFlavor::Tar,
        Some("bz2") => ArchiveFlavor::Other("bzip2"),
        Some("xz") => ArchiveFlavor::Other("xz"),
        Some("zst") => ArchiveFlavor::Other("zstandard"),
        Some("7z") => ArchiveFlavor::Other("7z"),
        Some("rar") => ArchiveFlavor::Other("rar"),
        Some("iso") => ArchiveFlavor::Other("ISO image"),
        _ => ArchiveFlavor::Other("unknown"),
    }
}

fn list_zip(path: &Path) -> ArchiveData {
    let Ok(file) = File::open(path) else {
        return ArchiveData::Unsupported("zip, unreadable".to_owned());
    };
    let Ok(mut archive) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        return ArchiveData::Unsupported("zip, unreadable".to_owned());
    };
    let total = archive.len();
    let mut entries = Vec::new();
    for index in 0..total.min(ARCHIVE_ENTRIES) {
        let Ok(entry) = archive.by_index(index) else {
            continue;
        };
        entries.push(ArchiveEntry {
            name: entry.name().to_owned(),
            size: entry.size(),
            is_dir: entry.is_dir(),
        });
    }
    ArchiveData::Listed(ArchiveListing {
        more: total > entries.len(),
        entries,
    })
}

fn list_tar(path: &Path, compressed: bool) -> ArchiveData {
    let Ok(file) = File::open(path) else {
        return ArchiveData::Unsupported("tar, unreadable".to_owned());
    };
    let reader: Box<dyn Read> = if compressed {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    let Ok(mut iter) = archive.entries() else {
        return ArchiveData::Unsupported("tar, unreadable".to_owned());
    };

    let mut entries = Vec::new();
    let mut more = false;
    while entries.len() < ARCHIVE_ENTRIES {
        match iter.next() {
            Some(Ok(entry)) => {
                let name = entry
                    .path()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let header = entry.header();
                entries.push(ArchiveEntry {
                    name,
                    size: header.size().unwrap_or(0),
                    is_dir: header.entry_type().is_dir(),
                });
            }
            Some(Err(_)) => break,
            None => break,
        }
    }
    if entries.len() == ARCHIVE_ENTRIES {
        // One more header tells us whether to say "and more".
        more = matches!(iter.next(), Some(Ok(_)));
    }
    if entries.is_empty() {
        let label = if compressed { "gzip" } else { "tar" };
        return ArchiveData::Unsupported(format!("{label}, not a tar archive"));
    }
    ArchiveData::Listed(ArchiveListing { entries, more })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::tempdir;

    use super::*;

    fn write(dir: &Path, name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn detection_prefers_content_over_extension() {
        let dir = tempdir().unwrap();
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R'];
        // Text masquerading as an image must not be treated as one.
        let fake = write(dir.path(), "fake.png", b"hello, this is plain text\n");
        assert_eq!(detect(&fake), Some(PreviewKind::Text));
        // A real image under a misleading name is still an image.
        let named_text = write(dir.path(), "actually-an-image.txt", png);
        assert_eq!(detect(&named_text), Some(PreviewKind::Image));
        // A real image with no extension at all is an image.
        let extensionless = write(dir.path(), "picture", png);
        assert_eq!(detect(&extensionless), Some(PreviewKind::Image));
        // A text file named like an archive is text, not an archive.
        let text_zip = write(dir.path(), "notes.zip", b"just some notes\n");
        assert_eq!(detect(&text_zip), Some(PreviewKind::Text));
    }

    #[test]
    fn detection_classifies_documents_and_archives() {
        let dir = tempdir().unwrap();
        let cases: &[(&str, &[u8], PreviewKind)] = &[
            ("a.json", br#"{"a": 1}"#, PreviewKind::Json),
            ("a.toml", b"a = 1\n", PreviewKind::Toml),
            ("a.yaml", b"a: 1\n", PreviewKind::Yaml),
            ("a.csv", b"a,b\n1,2\n", PreviewKind::Csv),
            ("a.tsv", b"a\tb\n1\t2\n", PreviewKind::Csv),
            ("a.txt", b"plain text\n", PreviewKind::Text),
            ("a.rs", b"fn main() {}\n", PreviewKind::Text),
            ("a.ts", b"const x: number = 1;\n", PreviewKind::Text),
            ("a.csv", b"a,b\n1,2\n", PreviewKind::Csv),
        ];
        for (name, contents, expected) in cases {
            let path = write(dir.path(), name, contents);
            assert_eq!(detect(&path), Some(*expected), "{name}");
        }
        assert_eq!(detect(dir.path()), None, "directories are not previews");
        assert_eq!(detect(Path::new("")), None);
    }

    #[test]
    fn detection_can_be_forced_by_extension_when_content_is_generic() {
        // A zero-length file has no content to sniff, so the name decides.
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "empty.json", b"");
        assert_eq!(detect(&path), Some(PreviewKind::Json));
    }

    #[test]
    fn lines_are_capped_and_note_more() {
        let text = "one\ntwo\nthree\nfour\n";
        let lines = lines_from(text, 2, Language::Plain);
        assert_eq!(lines.lines, vec!["one", "two"]);
        assert!(lines.more);

        let lines = lines_from(text, 99, Language::Plain);
        assert_eq!(lines.lines.len(), 4);
        assert!(!lines.more);
        assert!(lines.syntax.is_empty());
    }

    #[test]
    fn text_preview_carries_syntax_spans() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.rs");
        std::fs::write(&path, "fn main() { let x = 1; // c\n}\n").unwrap();
        let Some(DocumentData::Lines(lines)) = load_document(&path, PreviewKind::Text) else {
            panic!("expected text lines");
        };
        assert!(!lines.syntax.is_empty());
        // The first span is the `fn` keyword.
        assert_eq!(lines.syntax[0].start, 0);
        assert_eq!(lines.syntax[0].class, crate::highlight::TokenClass::Keyword);
        // A plain `.txt` file gets no spans.
        let plain = dir.path().join("note.txt");
        std::fs::write(&plain, "just text\n").unwrap();
        let Some(DocumentData::Lines(lines)) = load_document(&plain, PreviewKind::Text) else {
            panic!("expected text lines");
        };
        assert!(lines.syntax.is_empty());
    }

    #[test]
    fn decode_text_honours_utf16_bom_and_drops_nuls() {
        // UTF-8 passes through unchanged.
        assert_eq!(decode_text(b"plain text\n"), "plain text\n");

        // UTF-16LE with a BOM (a Windows desktop.ini): decoded, not NUL-riddled.
        let mut le = vec![0xFF, 0xFE];
        for unit in "[.ShellClassInfo]\r\n".encode_utf16() {
            le.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_text(&le), "[.ShellClassInfo]\r\n");

        // UTF-16BE with a BOM.
        let mut be = vec![0xFE, 0xFF];
        for unit in "hé".encode_utf16() {
            be.extend_from_slice(&unit.to_be_bytes());
        }
        assert_eq!(decode_text(&be), "hé");

        // A stray NUL in otherwise-UTF-8 bytes is dropped (GTK would panic).
        assert_eq!(decode_text(b"a\0b"), "ab");
    }

    #[test]
    fn long_lines_are_truncated() {
        let long = "x".repeat(1000);
        let line = truncate_line(&long);
        assert!(line.chars().count() < 1000);
        assert!(line.ends_with('…'));
    }

    #[test]
    fn csv_fields_honour_quotes() {
        let fields = split_fields(r#"a,"b,c",d"#, ',');
        assert_eq!(fields, vec!["a", "b,c", "d"]);
        let fields = split_fields(r#""say ""hi""",x"#, ',');
        assert_eq!(fields, vec![r#"say "hi""#, "x"]);
    }

    #[test]
    fn table_sniffs_the_delimiter_and_limits_cells() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "grid.csv", b"a,b,c\n1,2,3\n");
        let Some(DocumentData::Table(table)) = load_document(&path, PreviewKind::Csv) else {
            panic!("expected a table");
        };
        assert_eq!(table.rows[0], vec!["a", "b", "c"]);
        assert_eq!(table.rows[1], vec!["1", "2", "3"]);

        // A tab-separated file with a comma-free first line still lines up.
        let path = write(dir.path(), "grid.csv", b"a\tb\tc\n1\t2\t3\n");
        let Some(DocumentData::Table(table)) = load_document(&path, PreviewKind::Csv) else {
            panic!("expected a table");
        };
        assert_eq!(table.rows[0], vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_status_reports_validity_and_line() {
        assert!(parse_status(r#"{"a": 1}"#, PreviewKind::Json).ok);
        let bad = parse_status(r#"{"a": }"#, PreviewKind::Json);
        assert!(!bad.ok);
        assert!(bad.detail.contains("line 1"), "{}", bad.detail);

        assert!(parse_status("a = 1\n", PreviewKind::Toml).ok);
        assert!(!parse_status("a = \n", PreviewKind::Toml).ok);

        assert!(parse_status("a: 1\n", PreviewKind::Yaml).ok);
        assert!(!parse_status("a: [1, 2\n", PreviewKind::Yaml).ok);
    }

    #[test]
    fn archive_listing_reads_zip_and_tar() {
        let dir = tempdir().unwrap();

        // zip
        let zip_path = dir.path().join("a.zip");
        let file = File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("hello.txt", options).unwrap();
        zip.write_all(b"hi").unwrap();
        // A Deflate-compressed member must be listed too (the reader needs the
        // `deflate` backend for it).
        let deflated = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("world.txt", deflated).unwrap();
        zip.write_all(b"deflate me").unwrap();
        zip.finish().unwrap();
        let ArchiveData::Listed(listing) = list_archive(&zip_path) else {
            panic!("expected a zip listing");
        };
        assert_eq!(listing.entries.len(), 2, "{listing:?}");
        assert_eq!(listing.entries[0].name, "hello.txt");
        assert_eq!(listing.entries[0].size, 2);
        assert_eq!(listing.entries[1].name, "world.txt");
        assert!(!listing.more);

        // plain tar
        let tar_path = dir.path().join("a.tar");
        let file = File::create(&tar_path).unwrap();
        let mut builder = tar::Builder::new(file);
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "hello.txt", &b"hi"[..]).unwrap();
        builder.finish().unwrap();
        let ArchiveData::Listed(listing) = list_archive(&tar_path) else {
            panic!("expected a tar listing");
        };
        assert_eq!(listing.entries[0].name, "hello.txt");

        // gzip'd tar
        let tgz_path = dir.path().join("a.tar.gz");
        let file = File::create(&tgz_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "hello.txt", &b"hi"[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let ArchiveData::Listed(listing) = list_archive(&tgz_path) else {
            panic!("expected a tar.gz listing");
        };
        assert_eq!(listing.entries[0].name, "hello.txt");
    }

    #[test]
    fn unsupported_archives_are_summarised() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.7z", b"7z\xbc\xaf\x27\x1c whatever");
        let ArchiveData::Unsupported(_) = list_archive(&path) else {
            panic!("expected an unsupported summary");
        };
    }
}

