//! HKCU file associations kept under fully reclaimable keys.

use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegEnumKeyExW, RegEnumValueW,
    RegGetValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW,
};
use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
use windows::core::{PCWSTR, PWSTR};

const PROGID: &str = "riv.AssocFile";
const CLASSES_PROGID_KEY: &str = "Software\\Classes\\riv.AssocFile";
const APPLICATION_ROOT_KEY: &str = "Software\\riv";
const CAPABILITIES_KEY: &str = "Software\\riv\\Capabilities";
const FILE_ASSOCIATIONS_KEY: &str = "Software\\riv\\Capabilities\\FileAssociations";
const REGISTERED_APPLICATIONS_KEY: &str = "Software\\RegisteredApplications";
const EXPLORER_FILE_EXTS_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts";

use crate::text::wide;

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
            &raw mut key,
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

fn registry_read_string(subkey: &str, value_name: &str) -> Option<String> {
    let subkey_wide = wide(subkey);
    let value_name_wide = wide(value_name);
    let mut buffer = [0u16; 512];
    let mut size = (buffer.len() * 2) as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            PCWSTR(value_name_wide.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if read != ERROR_SUCCESS {
        return None;
    }
    let length = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buffer[..length]))
}

fn registry_names(
    subkey: &str,
    enumerate: impl Fn(HKEY, u32, PWSTR, *mut u32) -> WIN32_ERROR,
) -> Vec<String> {
    let mut result = Vec::new();
    let subkey_wide = wide(subkey);
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            None,
            KEY_READ,
            &raw mut key,
        )
    };
    if opened != ERROR_SUCCESS {
        return result;
    }
    for index in 0.. {
        let mut name = [0u16; 256];
        let mut name_length = name.len() as u32;
        let enumerated = enumerate(key, index, PWSTR(name.as_mut_ptr()), &raw mut name_length);
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

fn registry_subkeys(subkey: &str) -> Vec<String> {
    registry_names(subkey, |key, index, name, name_length| unsafe {
        RegEnumKeyExW(key, index, Some(name), name_length, None, None, None, None)
    })
}

fn registry_delete_tree(subkey: &str) {
    let subkey_wide = wide(subkey);
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(subkey_wide.as_ptr()));
    }
}

fn registry_key_is_empty(subkey: &str) -> bool {
    let subkey_wide = wide(subkey);
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            None,
            KEY_READ,
            &raw mut key,
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
            Some(&raw mut subkey_count),
            None,
            None,
            Some(&raw mut value_count),
            None,
            None,
            None,
            None,
        );
        let _ = RegCloseKey(key);
    }
    subkey_count == 0 && value_count == 0
}

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

fn add_extension_association(extension: &str) {
    // Record first so residue never exists without a record.
    registry_set_string(FILE_ASSOCIATIONS_KEY, Some(extension), PROGID);
    registry_set_string(
        &format!("Software\\Classes\\{extension}\\OpenWithProgids"),
        Some(PROGID),
        "",
    );
}

fn remove_extension_association(extension: &str) {
    // Record last so a crash leaves the record pointing at the leftover residue.
    remove_extension_residue(extension);
    registry_delete_value(FILE_ASSOCIATIONS_KEY, extension);
}

/// Removes every ProgID trace for one extension, including a UserChoice default pointing at riv.
fn remove_extension_residue(extension: &str) {
    let open_with_progids = format!("Software\\Classes\\{extension}\\OpenWithProgids");
    registry_delete_value(&open_with_progids, PROGID);
    if registry_key_is_empty(&open_with_progids) {
        registry_delete_tree(&open_with_progids);
    }
    let extension_key = format!("Software\\Classes\\{extension}");
    if registry_key_is_empty(&extension_key) {
        registry_delete_tree(&extension_key);
    }
    remove_explorer_residue(extension);
}

fn remove_explorer_residue(extension: &str) {
    let explorer_extension_key = format!("{EXPLORER_FILE_EXTS_KEY}\\{extension}");
    registry_delete_value(
        &format!("{explorer_extension_key}\\OpenWithProgids"),
        PROGID,
    );
    let user_choice_key = format!("{explorer_extension_key}\\UserChoice");
    if registry_read_string(&user_choice_key, "ProgId").as_deref() == Some(PROGID) {
        registry_delete_tree(&user_choice_key);
    }
}

fn reclaim_all_registration() {
    for extension in registered_extensions() {
        remove_extension_residue(&extension);
    }
    // Explorer writes FileExts entries on its own.
    for name in registry_subkeys(EXPLORER_FILE_EXTS_KEY) {
        if name.starts_with('.') {
            remove_explorer_residue(&name);
        }
    }
    registry_delete_value(REGISTERED_APPLICATIONS_KEY, "riv");
    registry_delete_tree(CLASSES_PROGID_KEY);
    registry_delete_tree(APPLICATION_ROOT_KEY); // includes Capabilities and FileAssociations
}

pub fn registered_extensions() -> Vec<String> {
    registry_names(
        FILE_ASSOCIATIONS_KEY,
        |key, index, name, name_length| unsafe {
            RegEnumValueW(key, index, Some(name), name_length, None, None, None, None)
        },
    )
}

/// Syncs to the desired set; an empty list reclaims everything.
pub fn set_file_associations(extensions: &[String]) {
    if extensions.is_empty() {
        reclaim_all_registration();
    } else {
        let current = registered_extensions();
        ensure_application_registration();
        for extension in extensions {
            if !current.contains(extension) {
                add_extension_association(extension);
            }
        }
        for extension in &current {
            if !extensions.contains(extension) {
                remove_extension_association(extension);
            }
        }
    }
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}
