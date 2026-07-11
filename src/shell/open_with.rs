//! Open With — 셸 등록 핸들러 열거·실행·다른 앱 선택 (SPEC §6.4).
//!
//! 열거(추천 필터·자연 정렬·기본 앱 최상단·자기 자신/무효 제외)는 **백그라운드
//! 스레드**에서 수행하고(250ms 디바운스는 창 타이머), 결과는 Send 안전한 데이터
//! (이름·실행 파일 경로)만 UI로 넘긴다. 실행은 UI 스레드에서 실행 파일 경로로
//! 핸들러를 재열거해 매칭 — COM 아파트먼트 경계를 넘기지 않는다.
//! 앱 아이콘은 미표시(2026-07-11 — P14 예외 철회, 메뉴 기본 스타일 일원화).

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, IDataObject};
use windows::Win32::UI::Shell::{
    ASSOC_FILTER_RECOMMENDED, ASSOCF_INIT_IGNOREUNKNOWN, ASSOCSTR_EXECUTABLE, AssocQueryStringW,
    BHID_DataObject, IAssocHandler, IShellItem, OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OPENASINFO,
    SHAssocEnumHandlers, SHCreateItemFromParsingName, SHOpenWithDialog,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{HSTRING, PCWSTR, Result};

/// 열거 완료 통지 — lparam = Box<OpenWithList> (250ms 디바운스 후 백그라운드 결과)
pub const WM_APP_OPEN_WITH_LIST: u32 = WM_APP + 4;

pub struct OpenWithItem {
    pub display_name: String,
    pub executable_path: String,
}

pub struct OpenWithList {
    /// 대상 파일 — 스테일 결과 폐기용
    pub path: PathBuf,
    /// 기본 앱이 있으면 첫 항목 (메뉴에서 구분선으로 분리 — SPEC §6.4)
    pub has_default: bool,
    pub items: Vec<OpenWithItem>,
}

/// 백그라운드 열거 시작 — 완료 시 WM_APP_OPEN_WITH_LIST 게시 (SPEC §6.4)
pub fn enumerate_in_background(window: HWND, path: PathBuf) {
    let window_handle = window.0 as isize;
    std::thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let list = Box::new(enumerate(&path));
        let pointer = Box::into_raw(list);
        let posted = unsafe {
            PostMessageW(
                Some(HWND(window_handle as *mut core::ffi::c_void)),
                WM_APP_OPEN_WITH_LIST,
                WPARAM(0),
                LPARAM(pointer as isize),
            )
        };
        if posted.is_err() {
            drop(unsafe { Box::from_raw(pointer) });
        }
    });
}

fn enumerate(path: &Path) -> OpenWithList {
    let mut items = Vec::new();
    let own_executable = std::env::current_exe()
        .map(|exe| exe.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let default_executable = default_executable_for(path).unwrap_or_default();

    for handler in handlers_for(path) {
        let Some(executable_path) = handler_name(&handler) else {
            continue;
        };
        // 무효 항목(실행 파일 부재)·자기 자신 제외 (SPEC §6.4)
        if !Path::new(&executable_path).is_file()
            || executable_path.to_lowercase() == own_executable
        {
            continue;
        }
        let display_name = handler_ui_name(&handler).unwrap_or_else(|| executable_path.clone());
        items.push(OpenWithItem {
            display_name,
            executable_path,
        });
    }
    // 자연 정렬 후 기본 앱 최상단 (SPEC §6.4)
    items.sort_by(|a, b| natural_compare(&a.display_name, &b.display_name));
    let default_index = (!default_executable.is_empty()).then(|| {
        items.iter().position(|item| {
            item.executable_path
                .eq_ignore_ascii_case(&default_executable)
        })
    });
    let mut has_default = false;
    if let Some(Some(index)) = default_index {
        let default_item = items.remove(index);
        items.insert(0, default_item);
        has_default = true;
    }
    OpenWithList {
        path: path.to_path_buf(),
        has_default,
        items,
    }
}

/// 항목 실행 — UI 스레드에서 실행 파일 경로로 재매칭 후 IAssocHandler::Invoke (SPEC §6.4)
pub fn invoke(path: &Path, executable_path: &str) -> Result<()> {
    for handler in handlers_for(path) {
        if handler_name(&handler).is_some_and(|name| name.eq_ignore_ascii_case(executable_path)) {
            unsafe {
                let item: IShellItem =
                    SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)?;
                let data_object: IDataObject = item.BindToHandler(None, &BHID_DataObject)?;
                return handler.Invoke(&data_object);
            }
        }
    }
    Ok(())
}

/// "다른 앱 선택" — OS Open With 다이얼로그 (SPEC §6.4)
pub fn show_open_with_dialog(window: HWND, path: &Path) {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let information = OPENASINFO {
        pcszFile: PCWSTR(wide.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_EXEC | OAIF_ALLOW_REGISTRATION,
    };
    let _ = unsafe { SHOpenWithDialog(Some(window), &information) };
}

fn handlers_for(path: &Path) -> Vec<IAssocHandler> {
    let Some(extension) = path.extension() else {
        return Vec::new();
    };
    let extension = HSTRING::from(format!(".{}", extension.to_string_lossy()));
    let Ok(enumerator) = (unsafe { SHAssocEnumHandlers(&extension, ASSOC_FILTER_RECOMMENDED) })
    else {
        return Vec::new();
    };
    let mut handlers = Vec::new();
    loop {
        let mut batch: [Option<IAssocHandler>; 8] = Default::default();
        let mut fetched = 0u32;
        if unsafe { enumerator.Next(&mut batch, Some(&mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        handlers.extend(batch.into_iter().take(fetched as usize).flatten());
    }
    handlers
}

fn handler_name(handler: &IAssocHandler) -> Option<String> {
    let name = unsafe { handler.GetName() }.ok()?;
    (!name.is_null()).then(|| String::from_utf16_lossy(unsafe { name.as_wide() }))
}

fn handler_ui_name(handler: &IAssocHandler) -> Option<String> {
    let name = unsafe { handler.GetUIName() }.ok()?;
    (!name.is_null()).then(|| String::from_utf16_lossy(unsafe { name.as_wide() }))
}

/// 기본 앱 실행 파일 — AssocQueryStringW(ASSOCSTR_EXECUTABLE)
fn default_executable_for(path: &Path) -> Option<String> {
    let extension = HSTRING::from(format!(".{}", path.extension()?.to_string_lossy()));
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    let status = unsafe {
        AssocQueryStringW(
            ASSOCF_INIT_IGNOREUNKNOWN,
            ASSOCSTR_EXECUTABLE,
            &extension,
            PCWSTR::null(),
            Some(windows::core::PWSTR(buffer.as_mut_ptr())),
            &mut length,
        )
    };
    (status.is_ok() && length > 1).then(|| String::from_utf16_lossy(&buffer[..length as usize - 1]))
}

/// 자연 정렬 (SPEC §6.4) — StrCmpLogicalW
fn natural_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use windows::Win32::UI::Shell::StrCmpLogicalW;
    let a_wide: Vec<u16> = a.encode_utf16().chain(std::iter::once(0)).collect();
    let b_wide: Vec<u16> = b.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe { StrCmpLogicalW(PCWSTR(a_wide.as_ptr()), PCWSTR(b_wide.as_ptr())) };
    result.cmp(&0)
}
