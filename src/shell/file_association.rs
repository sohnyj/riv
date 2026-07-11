//! 파일 연결 레지스트리 (SPEC §9 — HKCU 전용, qvwin32functions.cpp 연결 로직 이식)
//!
//! riv가 쓰는 키는 전부 회수 가능한 앵커 아래에 둔다:
//! - `HKCU\Software\Classes\riv.AssocFile` — ProgID(전부 riv 소유)
//! - `HKCU\Software\riv\Capabilities`(+`FileAssociations`) — 전부 riv 소유
//! - `HKCU\Software\RegisteredApplications` : `riv` 값 — 키는 공유, 값만 riv 소유
//! - `HKCU\Software\Classes\.<ext>\OpenWithProgids` : `riv.AssocFile` 값 —
//!   확장자당 값 하나, `.<ext>` 키는 타 앱과 공유
//!
//! 해제는 값 단위로 지우고, riv만 만든 빈 키는 프루닝·공유 키는 보존한다.
//! 전체 해제 시 ProgID·Capabilities·RegisteredApplications까지 완전 회수.
//! exe 경로는 등록 시점의 절대 경로 — 포터블 특성상 이동 시 재등록 필요.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteKeyW, RegDeleteTreeW, RegEnumValueW,
    RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW,
};
use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
use windows::core::PCWSTR;

const PROGID: &str = "riv.AssocFile";
const CLASSES_PROGID_KEY: &str = "Software\\Classes\\riv.AssocFile";
const APPLICATION_ROOT_KEY: &str = "Software\\riv";
const CAPABILITIES_KEY: &str = "Software\\riv\\Capabilities";
const FILE_ASSOCIATIONS_KEY: &str = "Software\\riv\\Capabilities\\FileAssociations";
const REGISTERED_APPLICATIONS_KEY: &str = "Software\\RegisteredApplications";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// REG_SZ 쓰기 — 값 이름 None = 기본값
fn registry_set_string(subkey: &str, value_name: Option<&str>, data: &str) {
    let subkey_wide = wide(subkey);
    let mut key = HKEY::default();
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if created != ERROR_SUCCESS {
        return;
    }
    let value_name_wide = value_name.map(wide);
    let data_wide = wide(data);
    let data_bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(data_wide.as_ptr().cast::<u8>(), data_wide.len() * 2) };
    unsafe {
        let _ = RegSetValueExW(
            key,
            value_name_wide
                .as_ref()
                .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr())),
            None,
            REG_SZ,
            Some(data_bytes),
        );
        let _ = RegCloseKey(key);
    }
}

fn registry_delete_value(subkey: &str, value_name: &str) {
    let subkey_wide = wide(subkey);
    let value_name_wide = wide(value_name);
    unsafe {
        let _ = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            PCWSTR(value_name_wide.as_ptr()),
        );
    }
}

/// 하위 트리 비운 뒤 (빈) 키 자체 제거
fn registry_delete_tree(subkey: &str) {
    let subkey_wide = wide(subkey);
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(subkey_wide.as_ptr()));
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(subkey_wide.as_ptr()));
    }
}

/// 값·하위 키가 전무한지 — 부재 키는 false(프루닝 대상 아님)
fn registry_key_is_empty(subkey: &str) -> bool {
    let subkey_wide = wide(subkey);
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
    };
    if opened != ERROR_SUCCESS {
        return false;
    }
    let mut subkey_count = 0u32;
    let mut value_count = 0u32;
    unsafe {
        let _ = RegQueryInfoKeyW(
            key,
            None,
            None,
            None,
            Some(&mut subkey_count),
            None,
            None,
            Some(&mut value_count),
            None,
            None,
            None,
            None,
        );
        let _ = RegCloseKey(key);
    }
    subkey_count == 0 && value_count == 0
}

/// ProgID·Capabilities·RegisteredApplications 등록 (SPEC §9).
/// ProgID 기본값(형식 표시명)은 **의도적으로 안 쓴다**(2026-07-11 결정) — riv를 기본
/// 앱으로 지정해도 유형 열은 Windows 폴백("PNG 파일" 등 확장자별)을 유지한다.
fn ensure_application_registration() {
    let executable = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    registry_set_string(
        &format!("{CLASSES_PROGID_KEY}\\DefaultIcon"),
        None,
        &format!("\"{executable}\",0"),
    );
    registry_set_string(
        &format!("{CLASSES_PROGID_KEY}\\shell\\open\\command"),
        None,
        &format!("\"{executable}\" \"%1\""),
    );
    registry_set_string(CAPABILITIES_KEY, Some("ApplicationName"), "riv");
    registry_set_string(
        CAPABILITIES_KEY,
        Some("ApplicationDescription"),
        "riv image viewer",
    );
    registry_set_string(REGISTERED_APPLICATIONS_KEY, Some("riv"), CAPABILITIES_KEY);
}

/// `extension`은 ".png" 형태
fn add_extension_association(extension: &str) {
    registry_set_string(
        &format!("Software\\Classes\\{extension}\\OpenWithProgids"),
        Some(PROGID),
        "",
    );
    registry_set_string(FILE_ASSOCIATIONS_KEY, Some(extension), PROGID);
}

fn remove_extension_association(extension: &str) {
    let open_with_progids = format!("Software\\Classes\\{extension}\\OpenWithProgids");
    registry_delete_value(&open_with_progids, PROGID);
    registry_delete_value(FILE_ASSOCIATIONS_KEY, extension);
    // riv만 만들어서 비게 된 키는 프루닝, 공유 키는 보존 (SPEC §9)
    if registry_key_is_empty(&open_with_progids) {
        registry_delete_tree(&open_with_progids);
    }
    let extension_key = format!("Software\\Classes\\{extension}");
    if registry_key_is_empty(&extension_key) {
        registry_delete_tree(&extension_key);
    }
}

/// 전체 해제 — 레지스트리 잔재 0 (SPEC §9 게이트)
fn reclaim_all_registration() {
    for extension in registered_extensions() {
        remove_extension_association(&extension);
    }
    registry_delete_tree(CLASSES_PROGID_KEY);
    registry_delete_tree(APPLICATION_ROOT_KEY); // Capabilities·FileAssociations 포함
    registry_delete_value(REGISTERED_APPLICATIONS_KEY, "riv");
}

/// 현재 등록된 확장자(".png" 형태) — FileAssociations 값 이름 열거.
/// 연결 UI의 초기 체크 상태이자 동기화 기준 (SPEC §8.3 — 기본값 개념 없음).
pub fn registered_extensions() -> Vec<String> {
    let mut result = Vec::new();
    let key_wide = wide(FILE_ASSOCIATIONS_KEY);
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_wide.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
    };
    if opened != ERROR_SUCCESS {
        return result;
    }
    for index in 0.. {
        let mut name = [0u16; 256];
        let mut name_length = name.len() as u32;
        let enumerated = unsafe {
            RegEnumValueW(
                key,
                index,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut name_length,
                None,
                None,
                None,
                None,
            )
        };
        if enumerated != ERROR_SUCCESS {
            break;
        }
        result.push(String::from_utf16_lossy(&name[..name_length as usize]));
    }
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

/// 원하는 연결 상태로 동기화 — 빈 목록 = 완전 회수. 변경 시 `SHChangeNotify` (SPEC §9)
pub fn set_file_associations(extensions: &[String]) {
    let current = registered_extensions();
    let desired = extensions;
    if current.len() == desired.len() && current.iter().all(|extension| desired.contains(extension))
    {
        return;
    }

    if desired.is_empty() {
        reclaim_all_registration();
    } else {
        ensure_application_registration();
        for extension in desired {
            if !current.contains(extension) {
                add_extension_association(extension);
            }
        }
        for extension in &current {
            if !desired.contains(extension) {
                remove_extension_association(extension);
            }
        }
    }

    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}
