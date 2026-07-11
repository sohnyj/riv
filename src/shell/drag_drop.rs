//! OLE drop target; accepts CF_HDROP paths only.

use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, LPARAM, POINTL, WPARAM};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
    RegisterDragDrop, ReleaseStgMedium,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{Result, implement};

pub const WM_APP_DROP_PATH: u32 = WM_APP + 3;

#[implement(IDropTarget)]
struct DropTarget {
    window: HWND,
}

pub fn register(window: HWND) -> Result<IDropTarget> {
    let target: IDropTarget = DropTarget { window }.into();
    unsafe { RegisterDragDrop(window, &target)? };
    Ok(target)
}

fn drop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn has_paths(data_object: Option<&IDataObject>) -> bool {
    data_object.is_some_and(|data| unsafe { data.QueryGetData(&drop_format()) }.is_ok())
}

fn dropped_paths(data_object: Option<&IDataObject>) -> Vec<PathBuf> {
    let Some(data) = data_object else {
        return Vec::new();
    };
    let Ok(mut medium) = (unsafe { data.GetData(&drop_format()) }) else {
        return Vec::new();
    };
    let drop_handle = HDROP(unsafe { medium.u.hGlobal }.0);
    let count = unsafe { DragQueryFileW(drop_handle, 0xFFFF_FFFF, None) };
    let mut paths = Vec::new();
    for index in 0..count {
        let mut buffer = [0u16; 32768];
        let length = unsafe { DragQueryFileW(drop_handle, index, Some(&mut buffer)) };
        if length > 0 {
            paths.push(PathBuf::from(String::from_utf16_lossy(
                &buffer[..length as usize],
            )));
        }
    }
    unsafe { ReleaseStgMedium(&raw mut medium) };
    paths
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        data_object: windows_core::Ref<'_, IDataObject>,
        _key_state: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe {
            *effect = if has_paths(data_object.as_ref()) {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE // refuse non-file drops (URLs etc.)
            };
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _key_state: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe {
            if *effect != DROPEFFECT_NONE {
                *effect = DROPEFFECT_COPY;
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        data_object: windows_core::Ref<'_, IDataObject>,
        _key_state: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let paths = dropped_paths(data_object.as_ref());
        if !paths.is_empty() {
            let pointer = Box::into_raw(Box::new(paths));
            let posted = unsafe {
                PostMessageW(
                    Some(self.window),
                    WM_APP_DROP_PATH,
                    WPARAM(0),
                    LPARAM(pointer as isize),
                )
            };
            if posted.is_err() {
                drop(unsafe { Box::from_raw(pointer) });
            }
            unsafe { *effect = DROPEFFECT_COPY };
        } else {
            unsafe { *effect = DROPEFFECT_NONE };
        }
        Ok(())
    }
}
