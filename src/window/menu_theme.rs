//! Dark context menus via the undocumented uxtheme SetPreferredAppMode ordinal.

use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{PCSTR, w};

type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;

/// AllowDark follows the system app theme, matching the title bar.
const PREFERRED_APP_MODE_ALLOW_DARK: i32 = 1;

const SET_PREFERRED_APP_MODE_ORDINAL: usize = 135;

/// Call once per process before the first menu; failure keeps light menus.
pub fn enable_dark_menus() {
    let Ok(uxtheme) =
        (unsafe { LoadLibraryExW(w!("uxtheme.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) })
    else {
        return;
    };
    let Some(address) =
        (unsafe { GetProcAddress(uxtheme, PCSTR(SET_PREFERRED_APP_MODE_ORDINAL as *const u8)) })
    else {
        return;
    };
    let set_preferred_app_mode: SetPreferredAppMode = unsafe { std::mem::transmute(address) };
    unsafe { set_preferred_app_mode(PREFERRED_APP_MODE_ALLOW_DARK) };
}
