//! 파일 조작 — 휴지통/영구 삭제(IFileOperation)·rename·explorer select
//! (SPEC §6.4, PORTING_PLAN §3 매핑). 확인 다이얼로그는 TaskDialogIndirect.

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

/// Show in Explorer — `explorer /select,<경로>` (SPEC §6.4)
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

/// 삭제 확인 결과
pub struct DeleteConfirmation {
    pub confirmed: bool,
    /// "다시 묻지 않기" 체크 (휴지통 삭제만 — SPEC §6.4 `askdelete`)
    pub do_not_ask_again: bool,
}

/// 삭제 확인 TaskDialog — 영구 삭제는 항상, 휴지통은 `askdelete`일 때 호출자가 띄운다
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
        pButtons: &delete_button,
        nDefaultButton: IDYES.0,
        ..Default::default()
    };
    if !permanent {
        configuration.pszVerificationText = verification;
    }
    let mut pressed = IDCANCEL.0;
    let mut checked = windows::core::BOOL(0);
    let result =
        unsafe { TaskDialogIndirect(&configuration, Some(&mut pressed), None, Some(&mut checked)) };
    DeleteConfirmation {
        confirmed: result.is_ok() && pressed == IDYES.0,
        do_not_ask_again: checked.as_bool(),
    }
}

/// 휴지통(FOF_ALLOWUNDO) 또는 영구 삭제 — IFileOperation (SPEC §6.4, P15)
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

/// 이름 변경 — 같은 디렉터리 내 새 이름, 성공 시 새 경로 반환 (SPEC §6.4)
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
