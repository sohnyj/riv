//! 다크 컨텍스트 메뉴 (SPEC §6.1 — R10 예외, 2026-07-11). Win32 팝업 메뉴에는
//! 공식 다크 모드 API가 없어 uxtheme 비문서화 ordinal `SetPreferredAppMode`(#135)를
//! 사용한다 — mpv `video/out/w32_common.c`와 동일한 운용. AllowDark는 강제가 아니라
//! 시스템 앱 테마(AppsUseLightTheme) 추종이므로 타이틀바(dwm.rs)와 판정 소스가 같다.
//! R1(Windows 11+)에서 ordinal 시그니처가 보장되므로 mpv의 1809 빌드 가드는 두지
//! 않는다. 로드 실패(wine 등 uxtheme 부재)는 무시 — 라이트 메뉴 유지.

use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{PCSTR, w};

/// uxtheme ordinal 135 — PreferredAppMode 열거값을 받아 이전 값을 반환
type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;

/// PreferredAppMode::AllowDark — 시스템 앱 테마가 다크면 메뉴도 다크
const PREFERRED_APP_MODE_ALLOW_DARK: i32 = 1;

const SET_PREFERRED_APP_MODE_ORDINAL: usize = 135;

/// 프로세스 전역 1회 — 첫 메뉴 생성 전(main 시작 시) 호출
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
