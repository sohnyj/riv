pub mod context_menu;
pub mod dwm;
pub mod overlay;

use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

/// 작업 영역(주 모니터, 작업표시줄 제외) 중앙 배치 원점 (SPEC §6.1 — 기본 위치 =
/// 스크린 중앙, 2026-07-11). 조회 실패 시 None(호출자 기본 배치 유지).
pub fn work_area_centered_origin(width: i32, height: i32) -> Option<(i32, i32)> {
    let mut work_area = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut work_area).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()?;
    Some((
        work_area.left + (work_area.right - work_area.left - width).max(0) / 2,
        work_area.top + (work_area.bottom - work_area.top - height).max(0) / 2,
    ))
}
