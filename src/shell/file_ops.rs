//! File operations: recycle/permanent delete, rename, Explorer select.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{E_ABORT, HWND};
use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TDF_ALLOW_DIALOG_CANCELLATION, TaskDialogIndirect,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FileOperation, IFileOperation, ILFree,
    IShellItem, SHCreateItemFromParsingName, SHOpenFolderAndSelectItems, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{IDCANCEL, IDNO, IDYES};
use windows::core::{HSTRING, PCWSTR, Result, w};

use crate::actions::Action;

pub fn show_in_explorer(window: HWND, path: &Path) {
    if let Err(error) = select_in_explorer(path) {
        crate::dialogs::message::show_message(
            Some(window),
            Action::ShowInExplorer.label(),
            "Can't show the file in Explorer.",
            &error.to_string(),
            crate::dialogs::message::CLOSE_BUTTON,
        );
    }
}

fn select_in_explorer(path: &Path) -> Result<()> {
    let mut item_list = std::ptr::null_mut();
    unsafe { SHParseDisplayName(&HSTRING::from(path), None, &raw mut item_list, 0, None) }?;
    // With no item array, the folder argument names the item to select in its parent folder.
    let selected = unsafe { SHOpenFolderAndSelectItems(item_list, None, 0) };
    unsafe { ILFree(Some(item_list)) };
    selected
}

pub struct DeleteConfirmation {
    pub confirmed: bool,
    pub do_not_ask_again: bool,
}

/// `details` carries the file facts, one per line, the first being the name.
pub fn confirm_delete(window: HWND, details: &str, permanent: bool) -> DeleteConfirmation {
    let wording = delete_wording(permanent);
    let content = crate::dialogs::message::body_text(wording.question, details);
    let verification = wording.ask_again.then_some(w!("Don't ask again"));
    let answer = ask_yes_no(window, wording.title, &content, verification);
    DeleteConfirmation {
        confirmed: answer.yes,
        do_not_ask_again: answer.verification_checked,
    }
}

/// What the confirmation says; the title is the action's label, so the two deletes title apart.
struct DeleteWording {
    title: &'static str,
    question: &'static str,
    /// Only the recycle delete offers to stop asking; a permanent delete always asks.
    ask_again: bool,
}

fn delete_wording(permanent: bool) -> DeleteWording {
    if permanent {
        DeleteWording {
            title: Action::DeletePermanently.label(),
            question: "Permanently delete this file?",
            ask_again: false,
        }
    } else {
        DeleteWording {
            title: Action::Delete.label(),
            question: "Move this file to the Recycle Bin?",
            ask_again: true,
        }
    }
}

struct YesNoAnswer {
    yes: bool,
    verification_checked: bool,
}

/// A Yes/No task dialog centered on the owner, with an optional verification check box.
fn ask_yes_no(
    window: HWND,
    title: &str,
    content: &HSTRING,
    verification: Option<PCWSTR>,
) -> YesNoAnswer {
    let title = HSTRING::from(title);
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: IDYES.0,
            pszButtonText: crate::dialogs::message::YES_BUTTON,
        },
        TASKDIALOG_BUTTON {
            nButtonID: IDNO.0,
            pszButtonText: crate::dialogs::message::NO_BUTTON,
        },
    ];
    let mut configuration = TASKDIALOGCONFIG {
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        nDefaultButton: IDYES.0,
        ..crate::dialogs::message::configuration(Some(window), &title, content, &buttons)
    };
    if let Some(verification) = verification {
        configuration.pszVerificationText = verification;
    }
    let mut pressed = IDCANCEL.0;
    let mut checked = windows::core::BOOL(0);
    let dialog_result = unsafe {
        TaskDialogIndirect(
            &raw const configuration,
            Some(&raw mut pressed),
            None,
            Some(&raw mut checked),
        )
    };
    YesNoAnswer {
        yes: dialog_result.is_ok() && pressed == IDYES.0,
        verification_checked: checked.as_bool(),
    }
}

pub fn delete_file(window: HWND, path: &Path, permanent: bool) -> Result<()> {
    unsafe {
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)?;
        operation.SetOwnerWindow(window)?;
        // No FOF_NOERRORUI: the shell's own error dialog is the failure surface.
        let mut flags = FOF_NOCONFIRMATION | FOF_SILENT;
        if !permanent {
            flags |= FOF_ALLOWUNDO;
        }
        operation.SetOperationFlags(flags)?;
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
        operation.DeleteItem(&item, None)?;
        operation.PerformOperations()?;
        // PerformOperations reports success even when the shell aborted the deletion.
        if operation.GetAnyOperationsAborted()?.as_bool() {
            return Err(windows::core::Error::from_hresult(E_ABORT));
        }
        Ok(())
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
    let name_before_first_dot = name
        .split_once('.')
        .map_or(name, |(before, _)| before)
        .to_ascii_uppercase();
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
            "The new name isn't a valid file name.",
        ));
    }
    let destination = path.with_file_name(new_name);
    if destination.as_os_str() == path.as_os_str() {
        return Ok(destination);
    }
    // No replace flag: renaming onto an existing file must fail, not overwrite it.
    unsafe {
        MoveFileExW(
            &HSTRING::from(path),
            &HSTRING::from(destination.as_path()),
            MOVE_FILE_FLAGS(0),
        )
    }
    .map_err(std::io::Error::other)?;
    Ok(destination)
}

pub fn show_rename_error(window: HWND, error: &std::io::Error) {
    crate::dialogs::message::show_message(
        Some(window),
        "Rename",
        "Can't rename the file.",
        &error.to_string(),
        crate::dialogs::message::CLOSE_BUTTON,
    );
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
