//! OLE drop target; accepts CF_HDROP paths only.

use std::cell::Cell;
use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, POINTL};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
    RegisterDragDrop, ReleaseStgMedium,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::WM_APP;
use windows::core::{Result, implement};

pub const WM_APP_DROP_PATHS: u32 = WM_APP + 3;

#[implement(IDropTarget)]
struct DropTarget {
    window: HWND,
    /// DragEnter verdict; DragOver must not re-derive it from the source's effect mask.
    accepts_current_drag: Cell<bool>,
}

pub fn register(window: HWND) -> Result<IDropTarget> {
    let target: IDropTarget = DropTarget {
        window,
        accepts_current_drag: Cell::new(false),
    }
    .into();
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
    data_object
        .is_some_and(|data_object| unsafe { data_object.QueryGetData(&drop_format()) }.is_ok())
}

fn dropped_paths(data_object: Option<&IDataObject>) -> Vec<PathBuf> {
    let Some(data_object) = data_object else {
        return Vec::new();
    };
    let Ok(mut medium) = (unsafe { data_object.GetData(&drop_format()) }) else {
        return Vec::new();
    };
    let drop_handle = HDROP(unsafe { medium.u.hGlobal }.0);
    let count = unsafe { DragQueryFileW(drop_handle, 0xFFFF_FFFF, None) };
    let mut paths = Vec::new();
    // One buffer for the whole drop; a long path can reach 32767 wide characters.
    let mut buffer = vec![0u16; 32768];
    for index in 0..count {
        let length = unsafe { DragQueryFileW(drop_handle, index, Some(buffer.as_mut_slice())) };
        if length > 0 {
            paths.push(crate::text::path_from_wide(&buffer[..length as usize]));
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
        let accepts = has_paths(data_object.as_ref());
        self.accepts_current_drag.set(accepts);
        let drop_effect = if accepts {
            DROPEFFECT_COPY
        } else {
            DROPEFFECT_NONE
        };
        unsafe { *effect = drop_effect };
        Ok(())
    }

    fn DragOver(
        &self,
        _key_state: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let drop_effect = if self.accepts_current_drag.get() {
            DROPEFFECT_COPY
        } else {
            DROPEFFECT_NONE
        };
        unsafe { *effect = drop_effect };
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
            crate::window::message::post_boxed(
                self.window.0 as isize,
                WM_APP_DROP_PATHS,
                Box::new(paths),
            );
            unsafe { *effect = DROPEFFECT_COPY };
        } else {
            unsafe { *effect = DROPEFFECT_NONE };
        }
        Ok(())
    }
}
