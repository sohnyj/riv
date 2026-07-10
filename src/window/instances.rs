//! 멀티윈도우 · 인스턴스 관리 (SPEC §6.1·§6.5)
//!
//! - 프로세스 내 창 레지스트리 + 최근 활성 창(최대 5) 추적 — "빈 창 재사용 →
//!   없으면 새 창" 정책의 탐색 순서. 창 생성·정책 실행은 main이 담당한다.
//! - 단일 인스턴스: 네임드 뮤텍스 선점 판별 + `FindWindowW`(창 클래스 `riv`) +
//!   `WM_COPYDATA` 경로 전달. 멀티 모드(기본)는 실행마다 새 프로세스 = 새 창.

use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsIconic, SW_RESTORE, SendMessageW, SetForegroundWindow, ShowWindow, WM_COPYDATA,
};
use windows::core::w;

/// 최근 활성 창 추적 수 (SPEC §6.1)
const RECENT_ACTIVE_LIMIT: usize = 5;

/// WM_COPYDATA dwData — 열 파일 경로 (UTF-16, 종단 NUL 없이)
const COPYDATA_OPEN_PATH: usize = 1;

static WINDOWS: Mutex<Vec<isize>> = Mutex::new(Vec::new());
static RECENTLY_ACTIVE: Mutex<Vec<isize>> = Mutex::new(Vec::new());

pub fn register_window(window: HWND) {
    WINDOWS.lock().unwrap().push(window.0 as isize);
}

/// 반환 = 남은 창 수 (0이면 호출자가 메시지 루프 종료)
pub fn unregister_window(window: HWND) -> usize {
    let handle = window.0 as isize;
    RECENTLY_ACTIVE
        .lock()
        .unwrap()
        .retain(|entry| *entry != handle);
    let mut windows = WINDOWS.lock().unwrap();
    windows.retain(|entry| *entry != handle);
    windows.len()
}

/// WM_ACTIVATE(활성) — 최근 활성 목록 선두로 (최대 5)
pub fn note_window_activated(window: HWND) {
    let handle = window.0 as isize;
    let mut recent = RECENTLY_ACTIVE.lock().unwrap();
    recent.retain(|entry| *entry != handle);
    recent.insert(0, handle);
    recent.truncate(RECENT_ACTIVE_LIMIT);
}

pub fn all_windows() -> Vec<HWND> {
    WINDOWS
        .lock()
        .unwrap()
        .iter()
        .map(|handle| HWND(*handle as *mut _))
        .collect()
}

/// 빈 창 탐색 순서 — 최근 활성(최대 5) 우선, 나머지는 생성 순 (SPEC §6.1)
pub fn windows_by_recency() -> Vec<HWND> {
    let recent = RECENTLY_ACTIVE.lock().unwrap().clone();
    let mut ordered = recent.clone();
    for handle in WINDOWS.lock().unwrap().iter() {
        if !ordered.contains(handle) {
            ordered.push(*handle);
        }
    }
    ordered
        .into_iter()
        .map(|handle| HWND(handle as *mut _))
        .collect()
}

/// 창 전면 활성화 — 최소화면 복원 (SPEC §6.5 "전면 활성화")
pub fn bring_to_foreground(window: HWND) {
    unsafe {
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        }
        let _ = SetForegroundWindow(window);
    }
}

/// 단일 인스턴스 선점 (SPEC §6.5) — 반환 false = 기존 인스턴스에 위임 완료(즉시 종료).
/// 뮤텍스 핸들은 프로세스 수명 소유(의도적 미해제).
pub fn claim_single_instance(path: Option<&Path>) -> bool {
    let _mutex = unsafe { CreateMutexW(None, false, w!("Local\\riv-single-instance")) };
    if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return true;
    }
    // 선점 인스턴스의 창으로 위임 — 창을 못 찾으면(초기화 중 등) 이 프로세스가 연다
    let Ok(existing) = (unsafe { FindWindowW(w!("riv"), None) }) else {
        return true;
    };
    if let Some(path) = path {
        send_open_path(existing, path);
    } else {
        bring_to_foreground(existing);
    }
    false
}

/// WM_COPYDATA로 경로 전달 (SPEC §6.5) — 수신 측이 빈 창 재사용 정책으로 연다
pub fn send_open_path(window: HWND, path: &Path) {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let payload = COPYDATASTRUCT {
        dwData: COPYDATA_OPEN_PATH,
        cbData: (wide.len() * 2) as u32,
        lpData: wide.as_ptr().cast_mut().cast(),
    };
    unsafe {
        SendMessageW(
            window,
            WM_COPYDATA,
            None,
            Some(LPARAM(&raw const payload as isize)),
        )
    };
}

/// WM_COPYDATA 수신 — 열 경로 파싱. 처리했으면 Some(경로)
pub fn parse_open_path(lparam: LPARAM) -> Option<PathBuf> {
    let payload = unsafe { (lparam.0 as *const COPYDATASTRUCT).as_ref() }?;
    if payload.dwData != COPYDATA_OPEN_PATH || payload.lpData.is_null() {
        return None;
    }
    let units = unsafe {
        std::slice::from_raw_parts(payload.lpData.cast::<u16>(), payload.cbData as usize / 2)
    };
    Some(PathBuf::from(std::ffi::OsString::from_wide(units)))
}
