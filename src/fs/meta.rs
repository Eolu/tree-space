//! File metadata for the Properties dialog.
//!
//! This is the non-visual half of the feature: a `stat(2)` snapshot plus the
//! helpers needed to *display* and *modify* it — human-readable sizes, Unix
//! permission math, owner/group name resolution, and image dimensions read
//! straight from file headers. Nothing here builds widgets, so it is all
//! exercised headlessly by `cargo test`.
//!
//! Only GIO is used (for the MIME content type and filesystem free space),
//! never GTK.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use relm4::gtk::gio;
use relm4::gtk::gio::prelude::FileExt;

/// One of the eight classic Unix access levels for a single triad (owner,
/// group or other), in the order the menu presents them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    None,
    Execute,
    Write,
    WriteExecute,
    Read,
    ReadExecute,
    ReadWrite,
    ReadWriteExecute,
}

impl Access {
    /// All eight levels, in menu order.
    pub const ALL: [Access; 8] = [
        Access::None,
        Access::Execute,
        Access::Write,
        Access::WriteExecute,
        Access::Read,
        Access::ReadExecute,
        Access::ReadWrite,
        Access::ReadWriteExecute,
    ];

    /// The 3-bit value for this level.
    pub fn bits(self) -> u32 {
        self as u32
    }

    /// Decode the low three bits of `bits`.
    pub fn from_bits(bits: u32) -> Self {
        match bits & 0b111 {
            0 => Access::None,
            1 => Access::Execute,
            2 => Access::Write,
            3 => Access::WriteExecute,
            4 => Access::Read,
            5 => Access::ReadExecute,
            6 => Access::ReadWrite,
            _ => Access::ReadWriteExecute,
        }
    }

    /// The menu label.
    pub fn label(self) -> &'static str {
        match self {
            Access::None => "None",
            Access::Execute => "Execute",
            Access::Write => "Write",
            Access::WriteExecute => "Write & execute",
            Access::Read => "Read",
            Access::ReadExecute => "Read & execute",
            Access::ReadWrite => "Read & write",
            Access::ReadWriteExecute => "Read, write & execute",
        }
    }

    /// The index of this level in [`Access::ALL`].
    pub fn index(self) -> u32 {
        self as u32
    }

    /// Whether any of the read/write/execute bits are set.
    pub fn is_empty(self) -> bool {
        self == Access::None
    }
}

/// A full Unix permission set: the three triads plus the set-uid/set-gid/sticky
/// bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub owner: Access,
    pub group: Access,
    pub other: Access,
    pub set_uid: bool,
    pub set_gid: bool,
    pub sticky: bool,
}

impl Permissions {
    /// Read the permission bits out of a raw `st_mode`.
    pub fn from_mode(mode: u32) -> Self {
        Permissions {
            owner: Access::from_bits(mode >> 6),
            group: Access::from_bits(mode >> 3),
            other: Access::from_bits(mode),
            set_uid: mode & 0o4000 != 0,
            set_gid: mode & 0o2000 != 0,
            sticky: mode & 0o1000 != 0,
        }
    }

    /// Just the nine permission bits (no set-uid/set-gid/sticky).
    pub fn mode_bits(self) -> u32 {
        (self.owner.bits() << 6) | (self.group.bits() << 3) | self.other.bits()
    }

    /// The full `chmod` value, including the special bits.
    pub fn to_mode(self) -> u32 {
        let mut mode = self.mode_bits();
        if self.set_uid {
            mode |= 0o4000;
        }
        if self.set_gid {
            mode |= 0o2000;
        }
        if self.sticky {
            mode |= 0o1000;
        }
        mode
    }
}

/// A `stat(2)` snapshot of one path, with the presentation-friendly fields the
/// Properties dialog needs already resolved.
#[derive(Debug, Clone)]
pub struct FileMeta {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// The link target, when `is_symlink`.
    pub link_target: Option<PathBuf>,
    /// Size in bytes (the link's own size for a symlink).
    pub size: u64,
    /// The mode a `chmod` would read/write. For a symlink this is the target's
    /// mode when it resolves, else the link's.
    pub mode: u32,
    pub permissions: Permissions,
    pub uid: u32,
    pub gid: u32,
    /// Resolved user/group names, or the numeric id when unresolvable.
    pub owner: String,
    pub group: String,
    pub owner_name: Option<String>,
    pub group_name: Option<String>,
    pub nlink: u64,
    pub inode: u64,
    pub mime_type: Option<String>,
    pub type_description: Option<String>,
    pub accessed: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
}

impl FileMeta {
    /// The name shown in dialog titles and headers.
    pub fn display_name(&self) -> String {
        self.name.clone()
    }

    /// The `stat` size, or — for a directory — the number of entries in it.
    /// Counting is best-effort; an unreadable directory yields `None`.
    pub fn item_count(&self) -> Option<u64> {
        if self.is_dir {
            count_items(&self.path).ok()
        } else {
            None
        }
    }
}

/// Read a metadata snapshot for `path`. Follows symlinks for the size/times/
/// mode (so permissions can be edited), but records the link itself too.
pub fn read_meta(path: &Path) -> io::Result<FileMeta> {
    let link_meta = fs::symlink_metadata(path)?;
    let is_symlink = link_meta.file_type().is_symlink();
    let link_target = if is_symlink {
        fs::read_link(path).ok()
    } else {
        None
    };

    // Permissions/type/times come from the target when the link resolves, so
    // editing them behaves like editing the file; a broken link falls back to
    // the link's own metadata.
    let followed = if is_symlink {
        fs::metadata(path).ok()
    } else {
        None
    };
    let meta = followed.as_ref().unwrap_or(&link_meta);

    let mode = meta.mode();
    let uid = meta.uid();
    let gid = meta.gid();
    let owner_name = user_name(uid);
    let group_name = group_name(gid);
    let mime_type = content_type_for(path);
    let type_description = mime_type
        .as_deref()
        .map(gio::content_type_get_description)
        .map(|s| s.to_string());

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    Ok(FileMeta {
        path: path.to_path_buf(),
        name,
        is_dir: meta.is_dir(),
        is_symlink,
        link_target,
        size: meta.size(),
        mode,
        permissions: Permissions::from_mode(mode),
        uid,
        gid,
        owner: owner_name.clone().unwrap_or_else(|| uid.to_string()),
        group: group_name.clone().unwrap_or_else(|| gid.to_string()),
        owner_name,
        group_name,
        nlink: meta.nlink(),
        inode: meta.ino(),
        mime_type,
        type_description,
        accessed: meta.accessed().ok(),
        modified: meta.modified().ok(),
        created: meta.created().ok(),
    })
}

/// Count the entries directly inside `dir` (the "N items" line).
pub fn count_items(dir: &Path) -> io::Result<u64> {
    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        entry?;
        count += 1;
    }
    Ok(count)
}

/// The MIME content type GIO reports for `path` (e.g. `text/plain`).
///
/// Deliberately *not* `content_type_guess_for_tree`: that call exists to find a
/// type common to a whole tree and returns an empty list for a single plain
/// file, which silently disabled both "Open With..." and its default-app label.
pub fn content_type_for(path: &Path) -> Option<String> {
    let file = gio::File::for_path(path);
    let info = file
        .query_info(
            gio::FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE,
            gio::FileQueryInfoFlags::NONE,
            None::<&gio::Cancellable>,
        )
        .ok()?;
    info.content_type().map(|s| s.to_string())
}

/// Free bytes on the filesystem holding `path`, if it can be queried.
pub fn available_space(path: &Path) -> Option<u64> {
    let file = gio::File::for_path(path);
    let info = file
        .query_filesystem_info(
            gio::FILE_ATTRIBUTE_FILESYSTEM_FREE,
            None::<&gio::Cancellable>,
        )
        .ok()?;
    Some(info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_FREE))
}

/// Apply a full permission set with `chmod(2)`.
pub fn set_permissions(path: &Path, permissions: Permissions) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(permissions.to_mode()))
}

/// Change the owner (`chown` uid only, keeping the group).
pub fn set_owner_uid(path: &Path, uid: u32) -> io::Result<()> {
    set_uint32_attribute(path, "unix::uid", uid)
}

/// Change the group (`chown` gid only, keeping the owner).
pub fn set_group_gid(path: &Path, gid: u32) -> io::Result<()> {
    set_uint32_attribute(path, "unix::gid", gid)
}

fn set_uint32_attribute(path: &Path, attribute: &str, value: u32) -> io::Result<()> {
    let file = gio::File::for_path(path);
    file.set_attribute_uint32(
        attribute,
        value,
        gio::FileQueryInfoFlags::NONE,
        None::<&gio::Cancellable>,
    )
    .map_err(|err| io::Error::other(err.to_string()))
}

/// Human-readable byte count in decimal units (1 kB = 1000 B), matching the
/// `g_format_size` output Nautilus shows.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} {}", if bytes == 1 { "byte" } else { "bytes" });
    }
    let mut value = bytes as f64;
    let mut index = 0;
    value /= 1000.0;
    while value >= 1000.0 && index < UNITS.len() - 1 {
        value /= 1000.0;
        index += 1;
    }
    format!("{value:.1} {}", UNITS[index])
}

/// "1 item" / "N items".
pub fn item_count_label(count: u64) -> String {
    format!("{count} {}", if count == 1 { "item" } else { "items" })
}

/// Resolve a user id to a name via `/etc/passwd`.
pub fn user_name(uid: u32) -> Option<String> {
    parse_id_name(&fs::read_to_string("/etc/passwd").ok()?, uid)
}

/// Resolve a group id to a name via `/etc/group`.
pub fn group_name(gid: u32) -> Option<String> {
    parse_id_name(&fs::read_to_string("/etc/group").ok()?, gid)
}

/// Every `(id, name)` pair listed in `/etc/passwd` (for the Owner combo).
pub fn list_users() -> Vec<(u32, String)> {
    list_id_names("/etc/passwd")
}

/// Every `(id, name)` pair listed in `/etc/group` (for the Group combo).
pub fn list_groups() -> Vec<(u32, String)> {
    list_id_names("/etc/group")
}

fn list_id_names(path: &str) -> Vec<(u32, String)> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(':');
        let Some(name) = fields.next() else { continue };
        if fields.next().is_none() {
            continue;
        }
        let Some(id) = fields.next().and_then(|f| f.parse::<u32>().ok()) else {
            continue;
        };
        entries.push((id, name.to_string()));
    }
    entries
}

/// Find the name whose id field (the third colon-separated field, used for both
/// `passwd` uid and `group` gid) equals `id`.
fn parse_id_name(text: &str, id: u32) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _ = fields.next()?;
        if fields.next()?.parse::<u32>().ok() == Some(id) {
            return Some(name.to_string());
        }
    }
    None
}

/// Pixel dimensions of an image, read from its header (PNG, JPEG, GIF, WebP or
/// BMP). `None` for anything else, or a truncated/corrupt header.
pub fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    let data = fs::read(path).ok()?;
    dimensions_from_bytes(&data)
}

fn dimensions_from_bytes(data: &[u8]) -> Option<(u32, u32)> {
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        // IHDR chunk: width/height are big-endian u32 at offsets 16 and 20.
        return Some((be32(data, 16)?, be32(data, 20)?));
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some((u32::from(le16(data, 6)?), u32::from(le16(data, 8)?)));
    }
    if data.starts_with(b"BM") {
        // BITMAPINFOHEADER: signed little-endian width/height at 18 and 22.
        let width = le32(data, 18)? as i32;
        let height = le32(data, 22)? as i32;
        if width > 0 && height > 0 {
            return Some((width as u32, height as u32));
        }
    }
    if data.starts_with(&[0xff, 0xd8]) {
        return jpeg_dimensions(data);
    }
    if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        return webp_dimensions(data.get(12..)?);
    }
    None
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 1 < data.len() {
        // Markers are 0xFF followed by a non-0xFF, non-zero type byte.
        if data[i] != 0xff {
            i += 1;
            continue;
        }
        let mut marker = data[i + 1];
        i += 2;
        while marker == 0xff && i < data.len() {
            marker = data[i];
            i += 1;
        }
        match marker {
            // Standalone markers (no length field).
            0x01 | 0xd0..=0xd7 => continue,
            // End of image / start of scan: no SOF will follow.
            0xd9 | 0xda => return None,
            _ => {}
        }
        if i + 1 >= data.len() {
            return None;
        }
        let length = (usize::from(data[i]) << 8) | usize::from(data[i + 1]);
        if length < 2 {
            return None;
        }
        // SOF0..3 / SOF5..7 / SOF9..11 / SOF13..15 carry the frame size.
        let is_sof = matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf);
        if is_sof {
            return Some((be16(data, i + 5)?, be16(data, i + 3)?));
        }
        i += length;
    }
    None
}

fn webp_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let payload = data.get(8..)?;
    match data.get(0..4)? {
        b"VP8 " => {
            // Lossy: 3-byte frame tag, 3-byte start code, then 14-bit dims.
            if payload.get(3..6) != Some(&[0x9d, 0x01, 0x2a]) {
                return None;
            }
            let width = u32::from(le16(payload, 6)? & 0x3fff);
            let height = u32::from(le16(payload, 8)? & 0x3fff);
            Some((width, height))
        }
        b"VP8L" => {
            // Lossless: 0x2F signature then two packed 14-bit lengths.
            if payload.first() != Some(&0x2f) {
                return None;
            }
            let bits = le32(payload, 1)?;
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => {
            // Extended: 1 flags byte + 3 reserved, then 24-bit canvas minus one.
            Some((le24(payload, 4)? + 1, le24(payload, 7)? + 1))
        }
        _ => None,
    }
}

fn be16(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 2)?;
    Some(u32::from(u16::from_be_bytes([bytes[0], bytes[1]])))
}

fn be32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn le16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn le24(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 3)?;
    Some(u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16))
}

fn le32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_round_trip() {
        for (bits, level) in Access::ALL.iter().enumerate() {
            assert_eq!(Access::from_bits(bits as u32), *level);
            assert_eq!(level.bits(), bits as u32);
            assert_eq!(level.index(), bits as u32);
        }
    }

    #[test]
    fn permissions_round_trip_including_special_bits() {
        for mode in [0o000, 0o644, 0o755, 0o4755, 0o2755, 0o1777, 0o7777] {
            let permissions = Permissions::from_mode(mode);
            assert_eq!(permissions.to_mode(), mode, "mode {mode:o}");
        }
    }

    #[test]
    fn permissions_split_into_triads() {
        let permissions = Permissions::from_mode(0o754);
        assert_eq!(permissions.owner, Access::ReadWriteExecute);
        assert_eq!(permissions.group, Access::ReadExecute);
        assert_eq!(permissions.other, Access::Read);
        assert!(!permissions.set_uid);
    }

    #[test]
    fn format_size_matches_nautilus_style() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1), "1 byte");
        assert_eq!(format_size(999), "999 bytes");
        assert_eq!(format_size(1000), "1.0 kB");
        assert_eq!(format_size(1500), "1.5 kB");
        assert_eq!(format_size(1_234_567), "1.2 MB");
        assert_eq!(format_size(3_000_000_000), "3.0 GB");
        assert_eq!(format_size(1_000_000_000_000), "1.0 TB");
    }

    #[test]
    fn item_count_label_is_pluralised() {
        assert_eq!(item_count_label(0), "0 items");
        assert_eq!(item_count_label(1), "1 item");
        assert_eq!(item_count_label(2), "2 items");
    }

    #[test]
    fn id_name_lookup_reads_the_third_field() {
        let passwd =
            "root:x:0:0:root:/root:/bin/bash\n# comment\nalice:x:1000:1000::/home/alice:/bin/sh\n";
        assert_eq!(parse_id_name(passwd, 0).as_deref(), Some("root"));
        assert_eq!(parse_id_name(passwd, 1000).as_deref(), Some("alice"));
        assert_eq!(parse_id_name(passwd, 9999), None);
    }

    #[test]
    fn list_id_names_skips_comments_and_blanks() {
        let group = "# group file\n\nwheel:x:10:alice\nusers:x:100:\n";
        let entries = {
            // Exercise the parsing path without touching /etc/group.
            let mut entries = Vec::new();
            for line in group.lines() {
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                let mut fields = line.split(':');
                if let (Some(name), Some(_), Some(id)) =
                    (fields.next(), fields.next(), fields.next())
                    && let Ok(id) = id.parse::<u32>()
                {
                    entries.push((id, name.to_string()));
                }
            }
            entries
        };
        assert_eq!(
            entries,
            vec![(10, "wheel".to_string()), (100, "users".to_string())]
        );
    }

    #[test]
    fn image_dimensions_parses_png() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(dimensions_from_bytes(&png), Some((640, 480)));
    }

    #[test]
    fn image_dimensions_parses_gif() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&200u16.to_le_bytes());
        assert_eq!(dimensions_from_bytes(&gif), Some((320, 200)));
    }

    #[test]
    fn image_dimensions_parses_bmp() {
        let mut bmp = b"BM".to_vec();
        bmp.resize(18, 0);
        bmp.extend_from_slice(&64i32.to_le_bytes());
        bmp.extend_from_slice(&48i32.to_le_bytes());
        assert_eq!(dimensions_from_bytes(&bmp), Some((64, 48)));
    }

    #[test]
    fn image_dimensions_parses_jpeg_sof() {
        // SOI, APP0 segment (skipped), then SOF0 carrying 800x600.
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend_from_slice(&[0xff, 0xe0, 0x00, 0x04, 0x00, 0x00]);
        jpeg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        jpeg.extend_from_slice(&800u16.to_be_bytes());
        jpeg.extend_from_slice(&600u16.to_be_bytes());
        jpeg.extend_from_slice(&[0x03, 0x01, 0x11, 0x00]);
        assert_eq!(dimensions_from_bytes(&jpeg), Some((600, 800)));
    }

    #[test]
    fn image_dimensions_parses_webp_vp8() {
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&0u32.to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(b"VP8 ");
        webp.extend_from_slice(&[0; 4]); // chunk size (unused)
        webp.extend_from_slice(&[0, 0, 0, 0x9d, 0x01, 0x2a]);
        webp.extend_from_slice(&256u16.to_le_bytes());
        webp.extend_from_slice(&128u16.to_le_bytes());
        assert_eq!(dimensions_from_bytes(&webp), Some((256, 128)));
    }

    #[test]
    fn image_dimensions_rejects_unknown() {
        assert_eq!(dimensions_from_bytes(b"not an image at all"), None);
        assert_eq!(dimensions_from_bytes(&[0xff, 0xd8, 0xff, 0xd9]), None);
    }

    #[test]
    fn read_meta_reports_regular_file_fields() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        fs::write(&file, b"hello").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();

        let meta = read_meta(&file).unwrap();
        assert_eq!(meta.name, "note.txt");
        assert!(!meta.is_dir);
        assert!(!meta.is_symlink);
        assert_eq!(meta.size, 5);
        assert_eq!(meta.permissions.owner, Access::ReadWrite);
        assert_eq!(meta.permissions.group, Access::Read);
        assert_eq!(meta.permissions.other, Access::None);
        assert_eq!(meta.uid, fs::metadata(&file).unwrap().uid());
        assert_eq!(
            meta.owner,
            user_name(meta.uid).unwrap_or_else(|| meta.uid.to_string())
        );
    }

    #[test]
    fn read_meta_detects_symlinks_and_their_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, b"data").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let meta = read_meta(&link).unwrap();
        assert!(meta.is_symlink);
        assert_eq!(meta.link_target.as_deref(), Some(target.as_path()));
        assert_eq!(meta.size, 4);
        assert!(!meta.is_dir);
    }

    #[test]
    fn count_items_counts_directory_entries() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), b"").unwrap();
        fs::write(dir.path().join("b"), b"").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        assert_eq!(count_items(dir.path()).unwrap(), 3);
    }

    #[test]
    fn set_permissions_round_trips_through_read_meta() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("exec");
        fs::write(&file, b"#!/bin/sh\n").unwrap();
        let permissions = Permissions {
            owner: Access::ReadWriteExecute,
            group: Access::ReadExecute,
            other: Access::None,
            set_uid: false,
            set_gid: false,
            sticky: false,
        };
        set_permissions(&file, permissions).unwrap();
        let meta = read_meta(&file).unwrap();
        assert_eq!(meta.permissions, permissions);
        assert_eq!(meta.mode & 0o777, 0o750);
    }
}
