//! File operations: recycle/permanent delete, rename, Explorer select.

use std::os::windows::ffi::OsStrExt;

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TDCBF_CANCEL_BUTTON, TDCBF_CLOSE_BUTTON,
    TDF_ALLOW_DIALOG_CANCELLATION, TaskDialogIndirect,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FileOperation, IFileOperation, IShellItem,
    SHCreateItemFromParsingName, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{IDCANCEL, IDYES, SW_SHOWNORMAL};
use windows::core::{HSTRING, PCWSTR, Result, w};

pub fn show_in_explorer(path: &Path) {
    let argument = HSTRING::from(format!("/select,\"{}\"", path.display()));
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("explorer.exe"),
            &argument,
            None,
            SW_SHOWNORMAL,
        )
    };
}

pub struct DeleteConfirmation {
    pub confirmed: bool,
    pub do_not_ask_again: bool,
}

pub fn confirm_delete(window: HWND, path: &Path, permanent: bool) -> DeleteConfirmation {
    let file_name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let instruction = crate::text::wide(if permanent {
        "Permanently delete this file?"
    } else {
        "Move this file to the Recycle Bin?"
    });
    let content = crate::text::wide(&file_name);
    let delete_label = crate::text::wide("Delete");
    let delete_button = TASKDIALOG_BUTTON {
        nButtonID: IDYES.0,
        pszButtonText: PCWSTR(delete_label.as_ptr()),
    };
    let verification = w!("Do not ask again");
    let mut configuration = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: window,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: w!("riv"),
        pszMainInstruction: PCWSTR(instruction.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: 1,
        pButtons: &raw const delete_button,
        nDefaultButton: IDYES.0,
        ..Default::default()
    };
    if !permanent {
        configuration.pszVerificationText = verification;
    }
    let mut pressed = IDCANCEL.0;
    let mut checked = windows::core::BOOL(0);
    let result = unsafe {
        TaskDialogIndirect(
            &raw const configuration,
            Some(&raw mut pressed),
            None,
            Some(&raw mut checked),
        )
    };
    DeleteConfirmation {
        confirmed: result.is_ok() && pressed == IDYES.0,
        do_not_ask_again: checked.as_bool(),
    }
}

pub fn delete_file(path: &Path, permanent: bool) -> Result<()> {
    unsafe {
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)?;
        let mut flags = FOF_NOCONFIRMATION | FOF_SILENT;
        if !permanent {
            flags |= FOF_ALLOWUNDO;
        }
        operation.SetOperationFlags(flags)?;
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)?;
        operation.DeleteItem(&item, None)?;
        operation.PerformOperations()
    }
}

/// Rejects names that would move the file, alias it to another, or hit a device.
fn new_name_is_invalid(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    // Separators move the file; ':' is a stream/drive; the rest NTFS forbids.
    if name.contains(['\\', '/', ':', '<', '>', '"', '|', '?', '*'])
        || name.chars().any(|character| (character as u32) < 0x20)
    {
        return true;
    }
    // Reserved device names resolve regardless of directory (MS file-naming rules).
    let name_before_first_dot = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
        "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
    ];
    RESERVED.contains(&name_before_first_dot.as_str())
}

pub fn rename_file(path: &Path, new_name: &str) -> std::io::Result<PathBuf> {
    // Win32 strips trailing dots and spaces; trim first so the real target is validated.
    let new_name = new_name.trim_end_matches([' ', '.']);
    if new_name_is_invalid(new_name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "The new name is not a valid file name",
        ));
    }
    let destination = path.with_file_name(new_name);
    if destination
        .as_os_str()
        .encode_wide()
        .eq(path.as_os_str().encode_wide())
    {
        return Ok(destination);
    }
    // No replace flag: renaming onto an existing file must fail, not overwrite it.
    unsafe {
        MoveFileExW(
            &HSTRING::from(path.as_os_str()),
            &HSTRING::from(destination.as_os_str()),
            MOVE_FILE_FLAGS(0),
        )
    }
    .map_err(std::io::Error::other)?;
    Ok(destination)
}

pub fn show_rename_error(window: HWND, error: &std::io::Error) {
    let content = crate::text::wide(&error.to_string());
    let configuration = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: window,
        pszWindowTitle: w!("riv"),
        pszMainInstruction: w!("Cannot rename the file"),
        pszContent: PCWSTR(content.as_ptr()),
        dwCommonButtons: TDCBF_CLOSE_BUTTON,
        ..Default::default()
    };
    let _ = unsafe { TaskDialogIndirect(&raw const configuration, None, None, None) };
}

#[cfg(test)]
mod rename_name_tests {
    use super::*;

    #[test]
    fn separators_are_rejected() {
        assert!(new_name_is_invalid("..\\a.png"));
        assert!(new_name_is_invalid("sub/a.png"));
        assert!(new_name_is_invalid("/a.png"));
        assert!(!new_name_is_invalid("a.png"));
        assert!(!new_name_is_invalid("한글 이미지.png"));
    }

    #[test]
    fn streams_and_reserved_devices_are_rejected() {
        assert!(new_name_is_invalid("photo.png:hidden")); // alternate data stream
        assert!(new_name_is_invalid("NUL")); // reserved device
        assert!(new_name_is_invalid("nul.png")); // case- and extension-insensitive
        assert!(new_name_is_invalid("COM1"));
        assert!(new_name_is_invalid("COM0"));
        assert!(new_name_is_invalid("lpt¹.png")); // superscript variant is reserved too
        assert!(new_name_is_invalid("a<b>.png"));
        assert!(new_name_is_invalid(""));
        assert!(!new_name_is_invalid("com1x.png")); // not a reserved device
        assert!(!new_name_is_invalid("COM10.png")); // two digits, not reserved
        assert!(!new_name_is_invalid("my.photo.png"));
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed_before_renaming() {
        // Same file after the trim: the early no-op return, no filesystem touched.
        let renamed = rename_file(Path::new("dir\\photo.png"), "photo.png. ").expect("trimmed");
        assert_eq!(renamed, Path::new("dir\\photo.png"));
        // A name that trims to nothing is invalid, not an empty rename.
        assert!(rename_file(Path::new("dir\\photo.png"), " . .").is_err());
    }
}
