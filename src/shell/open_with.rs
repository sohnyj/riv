//! Open With handler enumeration (SHAssocEnumHandlers) on a background thread.

use std::path::Path;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::IDataObject;
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
        crate::initialize_multithreaded_com();
        let list = Box::new(enumerate(extension));
        crate::window::message::post_boxed(window_handle, WM_APP_OPEN_WITH_LIST, list);
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    });
}

fn enumerate(extension: String) -> OpenWithList {
    let dotted_extension = HSTRING::from(crate::text::dotted_extension(&extension));
    let own_executable = crate::executable_path().to_string_lossy().into_owned();
    let mut items = handler_items(&dotted_extension, &own_executable);
    items.sort_by(|a, b| crate::text::natural_order_text(&a.display_name, &b.display_name));
    let has_default = default_executable_for(&dotted_extension)
        .is_some_and(|executable| promote_default(&mut items, &executable));
    OpenWithList {
        extension,
        has_default,
        items,
    }
}

/// The registered handlers with a readable executable, except riv itself, named for display.
fn handler_items(dotted_extension: &HSTRING, own_executable: &str) -> Vec<OpenWithItem> {
    // Packaged apps have no readable file path, so only riv itself is filtered out.
    handlers_for(dotted_extension)
        .into_iter()
        .filter_map(|handler| {
            let executable_path = handler_executable_path(&handler)?;
            if executable_path.eq_ignore_ascii_case(own_executable) {
                return None;
            }
            let display_name = handler_ui_name(&handler).unwrap_or_else(|| executable_path.clone());
            Some(OpenWithItem {
                display_name,
                executable_path,
            })
        })
        .collect()
}

/// Moves the default handler to the front; false when it is not in the list.
fn promote_default(items: &mut Vec<OpenWithItem>, executable: &str) -> bool {
    let Some(index) = items
        .iter()
        .position(|item| item.executable_path.eq_ignore_ascii_case(executable))
    else {
        return false;
    };
    let default_item = items.remove(index);
    items.insert(0, default_item);
    true
}

/// The launch result; a missing handler means the cached menu list went stale.
pub enum InvokeOutcome {
    Invoked,
    HandlerMissing,
    Failed(windows::core::Error),
}

pub fn invoke(path: &Path, executable_path: &str) -> InvokeOutcome {
    let Some(handler) = matching_handler(path, executable_path) else {
        return InvokeOutcome::HandlerMissing;
    };
    let launched = (|| -> Result<()> {
        unsafe {
            let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
            let data_object: IDataObject = item.BindToHandler(None, &BHID_DataObject)?;
            handler.Invoke(&data_object)
        }
    })();
    match launched {
        Ok(()) => InvokeOutcome::Invoked,
        Err(error) => InvokeOutcome::Failed(error),
    }
}

/// The handler registered for the path's extension whose executable matches, re-enumerated now.
fn matching_handler(path: &Path, executable_path: &str) -> Option<IAssocHandler> {
    let extension = crate::text::lowercase_extension(path)?;
    let dotted_extension = HSTRING::from(crate::text::dotted_extension(&extension));
    handlers_for(&dotted_extension).into_iter().find(|handler| {
        handler_executable_path(handler)
            .is_some_and(|name| name.eq_ignore_ascii_case(executable_path))
    })
}

pub fn show_open_with_dialog(window: HWND, path: &Path) {
    let wide = HSTRING::from(path);
    let information = OPENASINFO {
        pcszFile: PCWSTR(wide.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_EXEC | OAIF_ALLOW_REGISTRATION,
    };
    // A failure shows the shell's own error UI; the app has nothing to add.
    let _ = unsafe { SHOpenWithDialog(Some(window), &raw const information) };
}

fn handlers_for(dotted_extension: &HSTRING) -> Vec<IAssocHandler> {
    let Ok(enumerator) =
        (unsafe { SHAssocEnumHandlers(dotted_extension, ASSOC_FILTER_RECOMMENDED) })
    else {
        return Vec::new();
    };
    let mut handlers = Vec::new();
    loop {
        const HANDLER_BATCH_COUNT: usize = 8;
        let mut batch: [Option<IAssocHandler>; HANDLER_BATCH_COUNT] = Default::default();
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

fn default_executable_for(dotted_extension: &HSTRING) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    let status = unsafe {
        AssocQueryStringW(
            ASSOCF_INIT_IGNOREUNKNOWN,
            ASSOCSTR_EXECUTABLE,
            dotted_extension,
            PCWSTR::null(),
            Some(windows::core::PWSTR(buffer.as_mut_ptr())),
            &raw mut length,
        )
    };
    (status.is_ok() && length > 1).then(|| String::from_utf16_lossy(&buffer[..length as usize - 1]))
}
