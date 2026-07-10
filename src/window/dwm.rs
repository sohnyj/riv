//! DWM 창 속성 (SPEC §6.2, P14 — 커스텀 프레임 없이 DWM 제공 속성만).
//! wine 등 DWM 부재 환경은 호출 실패 무시(형상 무영향).

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DWMWA_TRANSITIONS_FORCEDISABLED, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
};

fn set_attribute<T>(
    window: HWND,
    attribute: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE,
    value: &T,
) {
    let _ = unsafe {
        DwmSetWindowAttribute(
            window,
            attribute,
            (value as *const T).cast(),
            size_of::<T>() as u32,
        )
    };
}

/// 다크 타이틀바 (P14) — 시스템 앱 테마(AppsUseLightTheme)를 따라 적용
pub fn apply_title_bar_theme(window: HWND) {
    let dark: i32 = i32::from(system_apps_use_dark_theme());
    set_attribute(window, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark);
}

/// 전체화면 전환 보정 (SPEC §6.2) — 진입 시 전환 애니메이션 비활성 + 라운드 코너
/// 해제(Win11 1px 갭·최대화 애니메이션 제거), 나갈 때 원복
pub fn set_fullscreen_polish(window: HWND, fullscreen: bool) {
    let transitions_disabled: i32 = i32::from(fullscreen);
    set_attribute(
        window,
        DWMWA_TRANSITIONS_FORCEDISABLED,
        &transitions_disabled,
    );
    let corner_preference = if fullscreen {
        DWMWCP_DONOTROUND
    } else {
        DWMWCP_DEFAULT
    };
    set_attribute(window, DWMWA_WINDOW_CORNER_PREFERENCE, &corner_preference);
}

/// HKCU Themes\Personalize의 AppsUseLightTheme == 0 → 다크 (조회 실패 = 라이트)
fn system_apps_use_dark_theme() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::w;

    let mut value = 1u32;
    let mut size = size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    result == windows::Win32::Foundation::ERROR_SUCCESS && value == 0
}
