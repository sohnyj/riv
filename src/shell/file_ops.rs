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
    let instruction: Vec<u16> = if permanent {
        "Permanently delete this file?"
    } else {
        "Move this file to the Recycle Bin?"
    }
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
    let content: Vec<u16> = file_name.encode_utf16().chain(std::iter::once(0)).collect();
    let delete_label: Vec<u16> = "Delete".encode_utf16().chain(std::iter::once(0)).collect();
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

/// Path separators would silently turn a rename into a move.
fn new_name_is_invalid(name: &str) -> bool {
    name.contains(['\\', '/'])
}

pub fn rename_file(path: &Path, new_name: &str) -> std::io::Result<PathBuf> {
    if new_name_is_invalid(new_name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "The new name cannot contain \\ or /",
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
    let content: Vec<u16> = error
        .to_string()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
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
}
