//! Painting a dialog surface through a buffer.

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{GetCurrentObject, HDC, OBJ_FONT, SelectObject};
use windows::Win32::UI::Controls::{
    BPBF_COMPATIBLEBITMAP, BeginBufferedPaint, BufferedPaintInit, BufferedPaintUnInit,
    EndBufferedPaint,
};

/// Readies the buffered paint the dialogs draw through; pair it with `end_buffered_painting`.
pub fn begin_buffered_painting() {
    let _ = unsafe { BufferedPaintInit() };
}

/// Frees what `begin_buffered_painting` set up on this thread.
pub fn end_buffered_painting() {
    let _ = unsafe { BufferedPaintUnInit() };
}

/// Runs a paint through a buffer, in the target's own coordinates, so its passes land together.
pub fn draw_buffered(target: HDC, bounds: RECT, paint: impl FnOnce(HDC)) {
    let mut device = HDC::default();
    let buffer = unsafe {
        BeginBufferedPaint(
            target,
            &raw const bounds,
            BPBF_COMPATIBLEBITMAP,
            None,
            &raw mut device,
        )
    };
    if buffer == 0 {
        paint(target);
        return;
    }
    // The buffer starts with the stock font, not the one the control draws with.
    let font = unsafe { GetCurrentObject(target, OBJ_FONT) };
    let previous = unsafe { SelectObject(device, font) };
    paint(device);
    unsafe {
        SelectObject(device, previous);
        let _ = EndBufferedPaint(buffer, true);
    }
}
