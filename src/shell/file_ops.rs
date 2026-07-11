//! File operations: recycle/permanent delete, rename, Explorer select.

use std::os::windows::ffi::OsStrExt;

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TDCBF_CANCEL_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION,
    TaskDialogIndirect,
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

pub fn rename_file(path: &Path, new_name: &str) -> std::io::Result<PathBuf> {
    let destination = path.with_file_name(new_name);
    if destination
        .as_os_str()
        .encode_wide()
        .eq(path.as_os_str().encode_wide())
    {
        return Ok(destination);
    }
    std::fs::rename(path, &destination)?;
    Ok(destination)
}
