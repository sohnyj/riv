//! IFileOpenDialog wrapper; filters derive from the decoder registry plus archive extensions.

use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FileOpenDialog, IFileOpenDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, PCWSTR};

use crate::image;

pub fn show(window: HWND, initial_directory: Option<&str>) -> Vec<PathBuf> {
    select_files(window, initial_directory).unwrap_or_default()
}

/// A filter per format in the file association order, then the two catch-alls and where they begin.
fn filters() -> (Vec<(String, String)>, usize) {
    let formats = image::formats::sorted_format_groups();
    let mut filters = Vec::with_capacity(formats.len() + 2);
    for (name, extensions) in formats {
        let pattern = extensions
            .iter()
            .map(|extension| format!("*.{extension}"))
            .collect::<Vec<_>>()
            .join(";");
        filters.push((format!("{name} ({pattern})"), pattern));
    }
    let supported_position = filters.len();
    // The name leaves the patterns out: the list is long and the formats are right above it.
    let all_patterns = filters
        .iter()
        .map(|(_, pattern)| pattern.as_str())
        .collect::<Vec<_>>()
        .join(";");
    filters.push(("Supported files".to_string(), all_patterns));
    filters.push(("All files".to_string(), "*.*".to_string()));
    (filters, supported_position)
}

fn select_files(
    window: HWND,
    initial_directory: Option<&str>,
) -> windows::core::Result<Vec<PathBuf>> {
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)? };

    let (filter_texts, supported_position) = filters();
    let filter_texts: Vec<(HSTRING, HSTRING)> = filter_texts
        .into_iter()
        .map(|(name, pattern)| (HSTRING::from(name), HSTRING::from(pattern)))
        .collect();
    let filters: Vec<COMDLG_FILTERSPEC> = filter_texts
        .iter()
        .map(|(name, pattern)| COMDLG_FILTERSPEC {
            pszName: PCWSTR(name.as_ptr()),
            pszSpec: PCWSTR(pattern.as_ptr()),
        })
        .collect();
    unsafe {
        dialog.SetFileTypes(&filters)?;
        // One-based; the dialog opens on everything riv reads, not on a single format.
        dialog.SetFileTypeIndex(supported_position as u32 + 1)?;
        let options = dialog.GetOptions()?;
        dialog.SetOptions(options | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST)?;
        if let Some(directory) = initial_directory
            && let Ok(folder) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(directory), None)
        {
            let _ = dialog.SetFolder(&folder);
        }
        if dialog.Show(Some(window)).is_err() {
            return Ok(Vec::new()); // cancelled
        }
        let results = dialog.GetResults()?;
        let count = results.GetCount()?;
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let item = results.GetItemAt(index)?;
            let raw = item.GetDisplayName(SIGDN_FILESYSPATH)?;
            if !raw.is_null() {
                paths.push(PathBuf::from(String::from_utf16_lossy(raw.as_wide())));
                CoTaskMemFree(Some(raw.as_ptr().cast()));
            }
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    #[test]
    fn formats_lead_and_the_supported_filter_carries_them_all() {
        let (filters, supported) = filters();
        // Same order as the file association list, which sorts by format name.
        let names: Vec<&str> = filters.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names[0], "APNG (*.apng)");
        assert_eq!(names[1], "AVIF (*.avif)");
        assert_eq!(names[2], "Archive (*.zip;*.7z;*.rar;*.tar)");
        assert!(names.contains(&"HEIF (*.heic;*.heif;*.hif)"));
        assert_eq!(names[supported], "Supported files");
        assert_eq!(names[supported + 1], "All files");
        // The name drops the patterns, the filter itself keeps every one of them.
        let all_patterns = &filters[supported].1;
        assert!(all_patterns.contains("*.apng"));
        assert!(all_patterns.contains("*.cbz"));
        assert!(all_patterns.ends_with("*.webp"));
    }
}
