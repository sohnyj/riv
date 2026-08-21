//! Open With handler enumeration (SHAssocEnumHandlers) on a background thread.

use std::path::Path;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, IDataObject};
use windows::Win32::UI::Shell::{
    ASSOC_FILTER_RECOMMENDED, ASSOCF_INIT_IGNOREUNKNOWN, ASSOCSTR_EXECUTABLE, AssocQueryStringW,
    BHID_DataObject, IAssocHandler, IShellItem, OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OPENASINFO,
    SHAssocEnumHandlers, SHCreateItemFromParsingName, SHOpenWithDialog,
};
use windows::Win32::UI::WindowsAndMessaging::WM_APP;
use windows::core::{HSTRING, PCWSTR, Result};

pub const WM_APP_OPEN_WITH_LIST: u32 = WM_APP + 4;

pub struct OpenWithItem {
    pub display_name: String,
    pub executable_path: String,
}

pub struct OpenWithList {
    pub extension: String,
    pub has_default: bool,
    pub items: Vec<OpenWithItem>,
}

pub fn enumerate_in_background(window: HWND, extension: String) {
    let window_handle = window.0 as isize;
    std::thread::spawn(move || {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let list = Box::new(enumerate(extension));
        crate::window::message::post_boxed(window_handle, WM_APP_OPEN_WITH_LIST, list);
        if initialized {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    });
}

fn enumerate(extension: String) -> OpenWithList {
    let mut items = Vec::new();
    let own_executable = std::env::current_exe()
        .map(|exe| exe.to_string_lossy().into_owned())
        .unwrap_or_default();
    let default_executable = default_executable_for(&extension).unwrap_or_default();

    // Packaged apps have no readable file path, so only riv itself is filtered out.
    for handler in handlers_for(&extension) {
        let Some(executable_path) = handler_executable_path(&handler) else {
            continue;
        };
        if executable_path.eq_ignore_ascii_case(&own_executable) {
            continue;
        }
        let display_name = handler_ui_name(&handler).unwrap_or_else(|| executable_path.clone());
        items.push(OpenWithItem {
            display_name,
            executable_path,
        });
    }
    items.sort_by(|a, b| crate::text::natural_order_text(&a.display_name, &b.display_name));
    let default_index = (!default_executable.is_empty())
        .then(|| {
            items.iter().position(|item| {
                item.executable_path
                    .eq_ignore_ascii_case(&default_executable)
            })
        })
        .flatten();
    let mut has_default = false;
    if let Some(index) = default_index {
        let default_item = items.remove(index);
        items.insert(0, default_item);
        has_default = true;
    }
    OpenWithList {
        extension,
        has_default,
        items,
    }
}

pub fn invoke(path: &Path, executable_path: &str) -> Result<()> {
    let Some(extension) = crate::text::lowercase_extension(path) else {
        return Ok(());
    };
    for handler in handlers_for(&extension) {
        if handler_executable_path(&handler)
            .is_some_and(|name| name.eq_ignore_ascii_case(executable_path))
        {
            unsafe {
                let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
                let data_object: IDataObject = item.BindToHandler(None, &BHID_DataObject)?;
                return handler.Invoke(&data_object);
            }
        }
    }
    Ok(())
}

pub fn show_open_with_dialog(window: HWND, path: &Path) {
    let wide = HSTRING::from(path);
    let information = OPENASINFO {
        pcszFile: PCWSTR(wide.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_EXEC | OAIF_ALLOW_REGISTRATION,
    };
    let _ = unsafe { SHOpenWithDialog(Some(window), &raw const information) };
}

fn handlers_for(extension: &str) -> Vec<IAssocHandler> {
    let extension = HSTRING::from(format!(".{extension}"));
    let Ok(enumerator) = (unsafe { SHAssocEnumHandlers(&extension, ASSOC_FILTER_RECOMMENDED) })
    else {
        return Vec::new();
    };
    let mut handlers = Vec::new();
    loop {
        let mut batch: [Option<IAssocHandler>; 8] = Default::default();
        let mut fetched = 0u32;
        if unsafe { enumerator.Next(&mut batch, Some(&raw mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        handlers.extend(batch.into_iter().take(fetched as usize).flatten());
    }
    handlers
}

fn handler_executable_path(handler: &IAssocHandler) -> Option<String> {
    unsafe { handler.GetName() }
        .ok()
        .map(crate::text::take_task_memory_string)
}

fn handler_ui_name(handler: &IAssocHandler) -> Option<String> {
    unsafe { handler.GetUIName() }
        .ok()
        .map(crate::text::take_task_memory_string)
}

/// Reads then frees a CoTaskMem-allocated string.
fn default_executable_for(extension: &str) -> Option<String> {
    let extension = HSTRING::from(format!(".{extension}"));
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    let status = unsafe {
        AssocQueryStringW(
            ASSOCF_INIT_IGNOREUNKNOWN,
            ASSOCSTR_EXECUTABLE,
            &extension,
            PCWSTR::null(),
            Some(windows::core::PWSTR(buffer.as_mut_ptr())),
            &raw mut length,
        )
    };
    (status.is_ok() && length > 1).then(|| String::from_utf16_lossy(&buffer[..length as usize - 1]))
}
