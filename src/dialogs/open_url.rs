//! Open URL dialog: the shared text input with a wide empty field.

use windows::Win32::Foundation::HWND;

use crate::dialogs::text_input::{self, TextInputRequest};

pub fn show(window: HWND) -> Option<String> {
    text_input::show(
        window,
        &TextInputRequest {
            template: crate::dialogs::resource::IDD_OPEN_URL,
            initial_text: "",
            selection: None,
        },
    )
    .map(|url| url.trim().to_string())
    .filter(|url| !url.is_empty())
}

#[cfg(test)]
mod dialog_tests {
    use super::*;
    use crate::dialogs::modal::IDOK;
    use crate::dialogs::resource::IDC_TEXT_INPUT;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{GetDlgItem, SetDlgItemTextW, WM_COMMAND};
    use windows::core::w;

    #[test]
    #[ignore = "creates a real dialog window"]
    fn dialog_round_trips_the_entered_url() {
        let driver = std::thread::spawn(|| {
            use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
            // 15s to ride out a cold wine start; the E2E smoke test waits as long.
            for _ in 0..300 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                // The window is findable by its title before the template builds its controls.
                let Ok(dialog) = (unsafe { FindWindowW(None, w!("Open URL")) }) else {
                    continue;
                };
                if unsafe { GetDlgItem(Some(dialog), IDC_TEXT_INPUT) }.is_err() {
                    continue;
                }
                unsafe {
                    SetDlgItemTextW(dialog, IDC_TEXT_INPUT, w!("  http://127.0.0.1/test.png  "))
                        .expect("set edit text");
                    PostMessageW(Some(dialog), WM_COMMAND, WPARAM(IDOK), LPARAM(0))
                        .expect("post IDOK");
                }
                return;
            }
            panic!("the dialog never appeared");
        });
        let url = show(HWND::default());
        driver.join().expect("driver thread");
        assert_eq!(url.as_deref(), Some("http://127.0.0.1/test.png"));
    }
}
