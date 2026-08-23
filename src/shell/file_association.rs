//! HKCU file associations kept under fully reclaimable keys; every registry touch is best effort.

use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
use windows_registry::CURRENT_USER;

const PROGID: &str = "riv.AssocFile";
const CLASSES_KEY: &str = "Software\\Classes";
const APPLICATION_ROOT_KEY: &str = "Software\\riv";
const REGISTERED_APPLICATIONS_KEY: &str = "Software\\RegisteredApplications";
const EXPLORER_FILE_EXTS_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts";
const OPEN_WITH_PROGIDS: &str = "OpenWithProgids";

fn classes_progid_key() -> String {
    format!("{CLASSES_KEY}\\{PROGID}")
}

/// Nested under the application root: reclaim deletes them by deleting the root tree.
fn capabilities_key() -> String {
    format!("{APPLICATION_ROOT_KEY}\\Capabilities")
}

fn file_associations_key() -> String {
    format!("{}\\FileAssociations", capabilities_key())
}

fn extension_open_with_progids_key(extension: &str) -> String {
    format!("{CLASSES_KEY}\\{extension}\\{OPEN_WITH_PROGIDS}")
}

/// An empty name is the key's default value.
fn registry_set_string(subkey: &str, value_name: &str, data: &str) {
    if let Ok(key) = CURRENT_USER.create(subkey) {
        let _ = key.set_string(value_name, data);
    }
}

fn registry_delete_value(subkey: &str, value_name: &str) {
    // The plain open() requests read access, which cannot delete.
    if let Ok(key) = CURRENT_USER.options().write().open(subkey) {
        let _ = key.remove_value(value_name);
    }
}

fn registry_read_string(subkey: &str, value_name: &str) -> Option<String> {
    CURRENT_USER.open(subkey).ok()?.get_string(value_name).ok()
}

fn registry_subkeys(subkey: &str) -> Vec<String> {
    // The iterator borrows the key, so the key outlives the walk.
    let Ok(key) = CURRENT_USER.open(subkey) else {
        return Vec::new();
    };
    key.keys().map(Iterator::collect).unwrap_or_default()
}

fn registry_values(subkey: &str) -> Vec<String> {
    let Ok(key) = CURRENT_USER.open(subkey) else {
        return Vec::new();
    };
    key.values()
        .map(|values| values.map(|(name, _)| name).collect())
        .unwrap_or_default()
}

fn registry_delete_tree(subkey: &str) {
    let _ = CURRENT_USER.remove_tree(subkey);
}

fn registry_key_is_empty(subkey: &str) -> bool {
    let Ok(key) = CURRENT_USER.open(subkey) else {
        return false;
    };
    key.keys().is_ok_and(|mut names| names.next().is_none())
        && key.values().is_ok_and(|mut values| values.next().is_none())
}

fn ensure_application_registration() {
    let executable = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    registry_set_string(
        &format!("{}\\DefaultIcon", classes_progid_key()),
        "",
        &format!("\"{executable}\",0"),
    );
    registry_set_string(
        &format!("{}\\shell\\open\\command", classes_progid_key()),
        "",
        &format!("\"{executable}\" \"%1\""),
    );
    registry_set_string(
        &capabilities_key(),
        "ApplicationName",
        crate::APPLICATION_NAME,
    );
    registry_set_string(
        &capabilities_key(),
        "ApplicationDescription",
        crate::APPLICATION_DESCRIPTION,
    );
    registry_set_string(
        REGISTERED_APPLICATIONS_KEY,
        crate::APPLICATION_NAME,
        &capabilities_key(),
    );
}

fn add_extension_association(extension: &str) {
    // Record first so a leftover never exists without a record.
    registry_set_string(&file_associations_key(), extension, PROGID);
    registry_set_string(&extension_open_with_progids_key(extension), PROGID, "");
}

fn remove_extension_association(extension: &str) {
    // Record last so a crash leaves the record pointing at the leftovers.
    remove_extension_leftovers(extension);
    registry_delete_value(&file_associations_key(), extension);
}

/// Removes every ProgID trace for one extension, including a UserChoice default pointing at riv.
fn remove_extension_leftovers(extension: &str) {
    let open_with_progids = extension_open_with_progids_key(extension);
    registry_delete_value(&open_with_progids, PROGID);
    if registry_key_is_empty(&open_with_progids) {
        registry_delete_tree(&open_with_progids);
    }
    let extension_key = format!("{CLASSES_KEY}\\{extension}");
    if registry_key_is_empty(&extension_key) {
        registry_delete_tree(&extension_key);
    }
    remove_explorer_leftovers(extension);
}

fn remove_explorer_leftovers(extension: &str) {
    let explorer_extension_key = format!("{EXPLORER_FILE_EXTS_KEY}\\{extension}");
    registry_delete_value(
        &format!("{explorer_extension_key}\\{OPEN_WITH_PROGIDS}"),
        PROGID,
    );
    let user_choice_key = format!("{explorer_extension_key}\\UserChoice");
    if registry_read_string(&user_choice_key, "ProgId").as_deref() == Some(PROGID) {
        registry_delete_tree(&user_choice_key);
    }
}

fn reclaim_all_registration() {
    for extension in registered_extensions() {
        remove_extension_leftovers(&extension);
    }
    // Explorer writes FileExts entries on its own.
    for name in registry_subkeys(EXPLORER_FILE_EXTS_KEY) {
        if name.starts_with('.') {
            remove_explorer_leftovers(&name);
        }
    }
    registry_delete_value(REGISTERED_APPLICATIONS_KEY, crate::APPLICATION_NAME);
    registry_delete_tree(&classes_progid_key());
    registry_delete_tree(APPLICATION_ROOT_KEY); // includes Capabilities and FileAssociations
}

pub fn registered_extensions() -> Vec<String> {
    registry_values(&file_associations_key())
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
