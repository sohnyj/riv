//! Per-user Start Menu shortcut: riv.lnk in the Programs known folder.

use std::path::PathBuf;

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellLink,
};
use windows::core::{HSTRING, Interface, Result, w};

pub fn shortcut_path() -> Option<PathBuf> {
    let pointer =
        unsafe { SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None) }.ok()?;
    let folder = unsafe { pointer.to_string() };
    unsafe { CoTaskMemFree(Some(pointer.as_ptr().cast())) };
    Some(PathBuf::from(folder.ok()?).join("riv.lnk"))
}

pub fn shortcut_exists() -> bool {
    shortcut_path().is_some_and(|path| path.is_file())
}

/// The link targets the executable's absolute path; recreate after moving the exe.
pub fn create_shortcut() {
    let Some(path) = shortcut_path() else {
        return;
    };
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let result: Result<()> = (|| unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(executable.as_os_str()))?;
        link.SetDescription(w!("riv image viewer"))?;
        if let Some(directory) = executable.parent() {
            link.SetWorkingDirectory(&HSTRING::from(directory.as_os_str()))?;
        }
        let persist: IPersistFile = link.cast()?;
        persist.Save(&HSTRING::from(path.as_os_str()), true)
    })();
    let _ = result;
}

pub fn remove_shortcut() {
    if let Some(path) = shortcut_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod start_menu_tests {
    use super::*;

    #[test]
    #[ignore] // touches the real user profile; run explicitly under wine
    fn shortcut_roundtrip() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        remove_shortcut();
        assert!(!shortcut_exists());
        create_shortcut();
        assert!(shortcut_exists());
        remove_shortcut();
        assert!(!shortcut_exists());
    }
}
