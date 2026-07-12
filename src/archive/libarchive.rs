//! Runtime binding to the Windows in-box libarchive (archiveint.dll, 23H2+).

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{PCSTR, w};

pub enum Archive {}
pub enum ArchiveEntry {}

pub const ARCHIVE_EOF: c_int = 1;
pub const ARCHIVE_OK: c_int = 0;

/// archive_entry_filetype masks (AE_IFMT / AE_IFREG in archive_entry.h).
pub const FILETYPE_MASK: c_uint = 0o170000;
pub const FILETYPE_REGULAR: c_uint = 0o100000;

type NewFunction = unsafe extern "C" fn() -> *mut Archive;
type ArchiveResultFunction = unsafe extern "C" fn(*mut Archive) -> c_int;
type OpenFilenameFunction = unsafe extern "C" fn(*mut Archive, *const u16, usize) -> c_int;
type NextHeaderFunction = unsafe extern "C" fn(*mut Archive, *mut *mut ArchiveEntry) -> c_int;
type ReadDataFunction = unsafe extern "C" fn(*mut Archive, *mut c_void, usize) -> isize;
type ErrorStringFunction = unsafe extern "C" fn(*mut Archive) -> *const c_char;
type EntryTextFunction = unsafe extern "C" fn(*mut ArchiveEntry) -> *const u16;
type EntryNumberFunction = unsafe extern "C" fn(*mut ArchiveEntry) -> i64;
type EntryFlagFunction = unsafe extern "C" fn(*mut ArchiveEntry) -> c_int;
type EntryFiletypeFunction = unsafe extern "C" fn(*mut ArchiveEntry) -> c_uint;

/// Read-only slice of the libarchive C API (zip/7z/rar/rar5/tar, no filters).
pub struct Api {
    pub read_new: NewFunction,
    pub read_free: ArchiveResultFunction,
    pub read_support_format_zip: ArchiveResultFunction,
    pub read_support_format_7zip: ArchiveResultFunction,
    pub read_support_format_rar: ArchiveResultFunction,
    pub read_support_format_rar5: ArchiveResultFunction,
    pub read_support_format_tar: ArchiveResultFunction,
    pub read_open_filename_w: OpenFilenameFunction,
    pub read_next_header: NextHeaderFunction,
    pub read_data: ReadDataFunction,
    pub entry_pathname_w: EntryTextFunction,
    pub entry_size: EntryNumberFunction,
    pub entry_size_is_set: EntryFlagFunction,
    pub entry_mtime: EntryNumberFunction,
    pub entry_filetype: EntryFiletypeFunction,
    pub entry_is_data_encrypted: EntryFlagFunction,
    pub entry_is_metadata_encrypted: EntryFlagFunction,
    pub error_string: ErrorStringFunction,
    pub errno: ArchiveResultFunction,
}

/// None when archiveint.dll or any required export is missing.
pub fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load_api).as_ref()
}

fn load_api() -> Option<Api> {
    let library =
        unsafe { LoadLibraryExW(w!("archiveint.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.ok()?;
    type ExportAddress = unsafe extern "system" fn() -> isize;
    macro_rules! resolve {
        ($symbol:literal, $signature:ty) => {{
            let address =
                unsafe { GetProcAddress(library, PCSTR(concat!($symbol, "\0").as_ptr())) }?;
            unsafe { std::mem::transmute::<ExportAddress, $signature>(address) }
        }};
    }
    Some(Api {
        read_new: resolve!("archive_read_new", NewFunction),
        read_free: resolve!("archive_read_free", ArchiveResultFunction),
        read_support_format_zip: resolve!("archive_read_support_format_zip", ArchiveResultFunction),
        read_support_format_7zip: resolve!(
            "archive_read_support_format_7zip",
            ArchiveResultFunction
        ),
        read_support_format_rar: resolve!("archive_read_support_format_rar", ArchiveResultFunction),
        read_support_format_rar5: resolve!(
            "archive_read_support_format_rar5",
            ArchiveResultFunction
        ),
        read_support_format_tar: resolve!("archive_read_support_format_tar", ArchiveResultFunction),
        read_open_filename_w: resolve!("archive_read_open_filename_w", OpenFilenameFunction),
        read_next_header: resolve!("archive_read_next_header", NextHeaderFunction),
        read_data: resolve!("archive_read_data", ReadDataFunction),
        entry_pathname_w: resolve!("archive_entry_pathname_w", EntryTextFunction),
        entry_size: resolve!("archive_entry_size", EntryNumberFunction),
        entry_size_is_set: resolve!("archive_entry_size_is_set", EntryFlagFunction),
        entry_mtime: resolve!("archive_entry_mtime", EntryNumberFunction),
        entry_filetype: resolve!("archive_entry_filetype", EntryFiletypeFunction),
        entry_is_data_encrypted: resolve!("archive_entry_is_data_encrypted", EntryFlagFunction),
        entry_is_metadata_encrypted: resolve!(
            "archive_entry_is_metadata_encrypted",
            EntryFlagFunction
        ),
        error_string: resolve!("archive_error_string", ErrorStringFunction),
        errno: resolve!("archive_errno", ArchiveResultFunction),
    })
}
