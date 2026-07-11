//! IFileOpenDialog wrapper; filters derive from the decoder registry.

use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FileOpenDialog, IFileOpenDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::image::decode;

pub fn show(window: HWND, initial_directory: Option<&str>) -> Vec<PathBuf> {
    show_inner(window, initial_directory).unwrap_or_default()
}

fn show_inner(
    window: HWND,
    initial_directory: Option<&str>,
) -> windows::core::Result<Vec<PathBuf>> {
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)? };

    let patterns: Vec<String> = decode::supported_extensions()
        .map(|extension| format!("*.{extension}"))
        .collect();
    let display = HSTRING::from(format!("Supported Images ({})", patterns.join(" ")));
    let pattern = HSTRING::from(patterns.join(";"));
    let filters = [
        COMDLG_FILTERSPEC {
            pszName: PCWSTR(display.as_ptr()),
            pszSpec: PCWSTR(pattern.as_ptr()),
        },
        COMDLG_FILTERSPEC {
            pszName: w!("All Files (*)"),
            pszSpec: w!("*.*"),
        },
    ];
    unsafe {
        dialog.SetFileTypes(&filters)?;
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
