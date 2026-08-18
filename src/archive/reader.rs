//! Safe read-only archive access: enumerate members, extract one to memory.

use std::ffi::CStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::core::HSTRING;

use crate::archive::libarchive::{
    self, ARCHIVE_EOF, ARCHIVE_OK, Api, Archive, ArchiveEntry, FILETYPE_MASK, FILETYPE_REGULAR,
};

/// Extension groups parallel to decode::format_groups.
const FORMAT_GROUPS: &[(&str, &[&str])] = &[
    ("Archive", &["zip", "7z", "rar", "tar"]),
    ("Comic book archive", &["cbz", "cb7", "cbr", "cbt"]),
];

/// Uncompressed per-member ceiling; guards against decompression bombs.
const MAXIMUM_MEMBER_BYTES: u64 = 1 << 30;

/// libarchive read-ahead block for archive_read_open_filename_w.
const OPEN_BLOCK_BYTES: usize = 128 * 1024;

/// Extraction chunk; cancellation is checked between chunks.
const READ_BLOCK_BYTES: usize = 256 * 1024;

/// Initial reservation cap; the declared size is attacker controlled.
const MAXIMUM_MEMBER_RESERVATION_BYTES: u64 = 16 << 20;

pub fn format_groups() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    FORMAT_GROUPS.iter().copied()
}

fn is_archive_extension(extension: &str) -> bool {
    FORMAT_GROUPS
        .iter()
        .any(|(_, extensions)| extensions.contains(&extension))
}

/// Whether a path names an archive; the extension decides, as it does everywhere here.
pub fn path_is_archive(path: &Path) -> bool {
    crate::text::lowercase_extension(path)
        .as_deref()
        .is_some_and(is_archive_extension)
}

/// The same question for a URL, whose extension comes out of its last path segment.
pub fn url_is_archive(url: &str) -> bool {
    crate::network::curl::extension_lowercase(url)
        .is_some_and(|extension| is_archive_extension(&extension))
}

pub struct MemberInfo {
    /// Path inside the archive, forward slashes as stored.
    pub name: String,
    /// Uncompressed size; 0 when the header does not declare one.
    pub size: u64,
    pub modified: SystemTime,
}

pub struct ArchiveError {
    pub message: String,
    pub code: i32,
    pub cancelled: bool,
}

impl ArchiveError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 0,
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            message: "cancelled".to_string(),
            code: 0,
            cancelled: true,
        }
    }
}

/// Lists regular-file members; skips encrypted ones, fails on encrypted metadata.
pub fn enumerate(archive_path: &Path) -> Result<Vec<MemberInfo>, ArchiveError> {
    let mut reader = Reader::open(archive_path)?;
    let mut members = Vec::new();
    while let Some(entry) = reader.next_header()? {
        if unsafe { (reader.api.entry_is_metadata_encrypted)(entry) } != 0 {
            return Err(ArchiveError::new("Archive metadata is encrypted"));
        }
        if unsafe { (reader.api.entry_filetype)(entry) } & FILETYPE_MASK != FILETYPE_REGULAR
            || unsafe { (reader.api.entry_is_data_encrypted)(entry) } != 0
        {
            continue;
        }
        let Some(name) = entry_name(reader.api, entry) else {
            continue;
        };
        let size = if unsafe { (reader.api.entry_size_is_set)(entry) } != 0 {
            unsafe { (reader.api.entry_size)(entry) }.max(0) as u64
        } else {
            0
        };
        let modified_seconds = unsafe { (reader.api.entry_mtime)(entry) }.max(0) as u64;
        members.push(MemberInfo {
            name,
            size,
            // checked_add: a crafted PAX/GNU mtime overflows SystemTime (verified).
            modified: UNIX_EPOCH
                .checked_add(Duration::from_secs(modified_seconds))
                .unwrap_or(UNIX_EPOCH),
        });
    }
    Ok(members)
}

/// Extracts one member to memory; `member_name` must match an enumerate result.
pub fn read_member(
    archive_path: &Path,
    member_name: &str,
    cancellation: &AtomicBool,
) -> Result<Vec<u8>, ArchiveError> {
    let mut reader = Reader::open(archive_path)?;
    while let Some(entry) = reader.next_header()? {
        if entry_name(reader.api, entry).as_deref() != Some(member_name) {
            continue; // next_header skips the unread data
        }
        if unsafe { (reader.api.entry_is_data_encrypted)(entry) } != 0 {
            return Err(ArchiveError::new("Archive member is encrypted"));
        }
        let declared_size = unsafe { (reader.api.entry_size_is_set)(entry) != 0 }
            .then(|| unsafe { (reader.api.entry_size)(entry) }.max(0) as u64);
        if declared_size.is_some_and(|size| size > MAXIMUM_MEMBER_BYTES) {
            return Err(ArchiveError::new("Archive member exceeds the 1 GiB limit"));
        }
        return reader.read_entry_data(declared_size, cancellation);
    }
    Err(ArchiveError::new("Member no longer exists in the archive"))
}

fn entry_name(api: &Api, entry: *mut ArchiveEntry) -> Option<String> {
    let pathname = unsafe { (api.entry_pathname_w)(entry) };
    if pathname.is_null() {
        return None;
    }
    let mut length = 0usize;
    while unsafe { *pathname.add(length) } != 0 {
        length += 1;
    }
    Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(pathname, length)
    }))
}

/// One archive handle per call; libarchive objects must not cross threads.
struct Reader<'api> {
    api: &'api Api,
    handle: *mut Archive,
}

impl Reader<'_> {
    fn open(archive_path: &Path) -> Result<Self, ArchiveError> {
        let api = libarchive::api()
            .ok_or_else(|| ArchiveError::new("Archive support could not be loaded"))?;
        let handle = unsafe { (api.read_new)() };
        if handle.is_null() {
            return Err(ArchiveError::new("Archive reader allocation failed"));
        }
        let reader = Self { api, handle };
        let supports = [
            api.read_support_format_zip,
            api.read_support_format_7zip,
            api.read_support_format_rar,
            api.read_support_format_rar5,
            api.read_support_format_tar,
        ];
        for support in supports {
            if unsafe { support(handle) } != ARCHIVE_OK {
                return Err(reader.error("Archive format registration failed"));
            }
        }
        let wide_path = HSTRING::from(archive_path);
        if unsafe { (api.read_open_filename_w)(handle, wide_path.as_ptr(), OPEN_BLOCK_BYTES) }
            != ARCHIVE_OK
        {
            return Err(reader.error("Archive could not be opened"));
        }
        Ok(reader)
    }

    fn next_header(&mut self) -> Result<Option<*mut ArchiveEntry>, ArchiveError> {
        let mut entry: *mut ArchiveEntry = std::ptr::null_mut();
        match unsafe { (self.api.read_next_header)(self.handle, &raw mut entry) } {
            ARCHIVE_OK => Ok(Some(entry)),
            ARCHIVE_EOF => Ok(None),
            _ => Err(self.error("Archive header read failed")),
        }
    }

    fn read_entry_data(
        &mut self,
        declared_size: Option<u64>,
        cancellation: &AtomicBool,
    ) -> Result<Vec<u8>, ArchiveError> {
        let mut data = Vec::with_capacity(
            declared_size
                .unwrap_or(0)
                .min(MAXIMUM_MEMBER_RESERVATION_BYTES) as usize,
        );
        let mut block = vec![0u8; READ_BLOCK_BYTES];
        loop {
            if cancellation.load(Ordering::Relaxed) {
                return Err(ArchiveError::cancelled());
            }
            let read_bytes = unsafe {
                (self.api.read_data)(self.handle, block.as_mut_ptr().cast(), block.len())
            };
            if read_bytes == 0 {
                return Ok(data);
            }
            if read_bytes < 0 {
                return Err(self.error("Archive member extraction failed"));
            }
            if data.len() as u64 + read_bytes as u64 > MAXIMUM_MEMBER_BYTES {
                return Err(ArchiveError::new("Archive member exceeds the 1 GiB limit"));
            }
            data.extend_from_slice(&block[..read_bytes as usize]);
        }
    }

    fn error(&self, fallback: &str) -> ArchiveError {
        let text = unsafe { (self.api.error_string)(self.handle) };
        let message = if text.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(text) }
                .to_string_lossy()
                .into_owned()
        };
        ArchiveError {
            message: if message.trim().is_empty() {
                fallback.to_string()
            } else {
                message
            },
            code: unsafe { (self.api.errno)(self.handle) },
            cancelled: false,
        }
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.read_free)(self.handle) };
    }
}

#[cfg(test)]
mod extension_tests {
    use super::*;

    #[test]
    fn archive_extensions_cover_the_confirmed_scope() {
        for extension in ["zip", "7z", "rar", "tar", "cbz", "cb7", "cbr", "cbt"] {
            assert!(is_archive_extension(extension), "{extension}");
        }
        assert!(!is_archive_extension("png"));
        assert!(!is_archive_extension("gz")); // compressed tar is out of scope
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;

    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

    #[test]
    #[ignore = "needs archiveint.dll and test/ fixtures"]
    fn fixtures_enumerate_and_extract() {
        for fixture in [
            "test/fixture.zip",
            "test/fixture.cbz",
            "test/fixture.7z",
            "test/fixture.cb7",
            "test/fixture.tar",
            "test/fixture.cbt",
        ] {
            let members = enumerate(Path::new(fixture))
                .unwrap_or_else(|error| panic!("{fixture}: {}", error.message));
            assert_eq!(members.len(), 5, "{fixture}");
            let image = members
                .iter()
                .find(|member| member.name.ends_with("03.png"))
                .unwrap_or_else(|| panic!("{fixture}: nested member missing"));
            assert!(image.name.contains("art"), "{fixture}: {}", image.name);
            assert!(
                members
                    .iter()
                    .any(|member| member.name.contains("한글 이미지")),
                "{fixture}: unicode member missing"
            );
            let cancellation = AtomicBool::new(false);
            let data = read_member(Path::new(fixture), &image.name, &cancellation)
                .unwrap_or_else(|error| panic!("{fixture}: {}", error.message));
            assert!(data.starts_with(PNG_SIGNATURE), "{fixture}");
            if image.size > 0 {
                assert_eq!(data.len() as u64, image.size, "{fixture}");
            }
            let missing = read_member(Path::new(fixture), "no/such_member.png", &cancellation);
            assert!(missing.is_err(), "{fixture}");
        }
    }

    #[test]
    #[ignore = "needs archiveint.dll and test/ fixtures"]
    fn encrypted_zip_members_are_skipped() {
        let members = enumerate(Path::new("test/fixture_encrypted.zip"))
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert!(
            members.is_empty(),
            "data-encrypted members must not be listed"
        );
    }
}

#[cfg(test)]
mod mtime_tests {
    use super::*;

    #[test]
    #[ignore = "needs archiveint.dll and test/fixture_mtime_overflow.tar"]
    fn an_overflowing_member_mtime_clamps_instead_of_panicking() {
        let members = enumerate(Path::new("test/fixture_mtime_overflow.tar"))
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].modified, UNIX_EPOCH); // clamped fallback, no panic
    }

    // libarchive returns the declared size verbatim; the cap must hold.
    #[test]
    #[ignore = "needs archiveint.dll and test/fixture_zip_declared_1gib.zip"]
    fn a_member_declaring_a_huge_size_is_handled_gracefully() {
        let cancellation = AtomicBool::new(false);
        let result = read_member(
            Path::new("test/fixture_zip_declared_1gib.zip"),
            "huge.png",
            &cancellation,
        );
        assert!(result.is_err()); // no crash/hang; reservation stayed bounded
    }
}
