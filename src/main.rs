#![windows_subsystem = "windows"]

mod view;

use view::renderer::Renderer;
use view::transform::{Size, ViewTransform};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::D2D1_INTERPOLATION_MODE;
use windows::Win32::Graphics::Direct2D::{
    D2D1_INTERPOLATION_MODE_CUBIC, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
};
use windows::Win32::Graphics::Gdi::{
    COLOR_WINDOW, GetMonitorInfoW, GetSysColorBrush, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromWindow, ScreenToClient, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_ADD, VK_BACK, VK_DOWN, VK_F11, VK_LEFT, VK_OEM_MINUS, VK_OEM_PLUS, VK_RIGHT, VK_SUBTRACT,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    GWL_STYLE, GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowPlacement,
    HWND_TOP, IDC_ARROW, LoadCursorW, LoadIconW, MSG, PostQuitMessage, RegisterClassExW, SW_SHOW,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW,
    SetWindowPlacement, SetWindowPos, ShowWindow, TranslateMessage, WINDOW_STYLE, WINDOWPLACEMENT,
    WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_MOUSEWHEEL, WM_NCDESTROY, WM_PAINT, WM_SIZE,
    WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, Result, w};

// res/riv.rc의 아이콘 리소스 ID (MAKEINTRESOURCE — 정수 1을 포인터 슬롯에 싣는다)
const APPLICATION_ICON_ID: PCWSTR = PCWSTR(std::ptr::without_provenance(1));

// 기본 배경색 #212121 (SPEC §8.2 bgcolor — 설정 모듈은 R3)
const BACKGROUND_COLOR: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 0x21 as f32 / 255.0,
    g: 0x21 as f32 / 255.0,
    b: 0x21 as f32 / 255.0,
    a: 1.0,
};

struct Application {
    renderer: Renderer,
    view_transform: ViewTransform,
    image_pixels: Vec<u8>,
    image_width: u32,
    image_height: u32,
    /// 스케일링 티어 0..=3 (SPEC §3.3 — 설정 연동은 R3)
    scaling_tier: u32,
    /// 전체화면 진입 전 창 상태 (R1 임시 — DWM 보정은 R7)
    fullscreen_restore: Option<(WINDOWPLACEMENT, WINDOW_STYLE)>,
}

impl Application {
    fn new(window: HWND) -> Result<Self> {
        let (width, height) = client_size(window);
        let mut renderer = Renderer::new(window, width.max(1), height.max(1))?;
        let (image_pixels, image_width, image_height) = generate_test_texture();
        renderer.set_image(&image_pixels, image_width, image_height)?;
        let device_pixel_ratio = unsafe { GetDpiForWindow(window) } as f32 / 96.0;
        Ok(Self {
            renderer,
            view_transform: ViewTransform::new(device_pixel_ratio),
            image_pixels,
            image_width,
            image_height,
            scaling_tier: 1,
            fullscreen_restore: None,
        })
    }

    fn image_size(&self) -> Size {
        Size {
            width: self.image_width as f32,
            height: self.image_height as f32,
        }
    }

    fn interpolation_mode(&self) -> D2D1_INTERPOLATION_MODE {
        match self.scaling_tier {
            0 => D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            1 => D2D1_INTERPOLATION_MODE_LINEAR,
            2 => D2D1_INTERPOLATION_MODE_CUBIC,
            _ => D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
        }
    }

    /// 디바이스 로스트 시 전체 재구축 (SPEC §3.4)
    fn rebuild_renderer(&mut self, window: HWND) -> Result<()> {
        let (width, height) = client_size(window);
        self.renderer = Renderer::new(window, width.max(1), height.max(1))?;
        self.renderer
            .set_image(&self.image_pixels, self.image_width, self.image_height)
    }

    fn render(&mut self, window: HWND) {
        let (width, height) = client_size(window);
        if width == 0 || height == 0 {
            return;
        }
        let viewport = Size {
            width: width as f32,
            height: height as f32,
        };
        let image = self.image_size();
        self.view_transform.synchronize(viewport, image);
        let matrix = self.view_transform.matrix(viewport, image);
        let interpolation = self.interpolation_mode();
        if self
            .renderer
            .render(matrix, interpolation, BACKGROUND_COLOR)
            .is_err()
        {
            // 디바이스 로스트 — 재구축 후 1회 재시도
            if self.rebuild_renderer(window).is_ok() {
                let _ = self
                    .renderer
                    .render(matrix, interpolation, BACKGROUND_COLOR);
            }
        }
    }
}

fn client_size(window: HWND) -> (u32, u32) {
    let mut bounds = RECT::default();
    let _ = unsafe { GetClientRect(window, &mut bounds) };
    (
        (bounds.right - bounds.left).max(0) as u32,
        (bounds.bottom - bounds.top).max(0) as u32,
    )
}

/// R1 게이트 검증용 테스트 텍스처 — 회색 체커 + 방향 판별용 코너 마커
/// (좌상 빨강, 우상 초록, 좌하 파랑, 우하 노랑) + 중앙 크로스헤어 + 1px 테두리
fn generate_test_texture() -> (Vec<u8>, u32, u32) {
    let width = 640u32;
    let height = 400u32;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let index = ((y * width + x) * 4) as usize;
            let checker_tone = if (x / 32 + y / 32).is_multiple_of(2) {
                0x38
            } else {
                0x58
            };
            let (red, green, blue) = if x < 24 && y < 24 {
                (0xFF, 0x20, 0x20)
            } else if x >= width - 24 && y < 24 {
                (0x20, 0xFF, 0x20)
            } else if x < 24 && y >= height - 24 {
                (0x20, 0x40, 0xFF)
            } else if x >= width - 24 && y >= height - 24 {
                (0xFF, 0xE0, 0x20)
            } else if x == 0 || y == 0 || x == width - 1 || y == height - 1 {
                (0xFF, 0xFF, 0xFF)
            } else if x == width / 2 || y == height / 2 {
                (0xF0, 0xF0, 0xF0)
            } else {
                (checker_tone, checker_tone, checker_tone)
            };
            pixels[index] = blue;
            pixels[index + 1] = green;
            pixels[index + 2] = red;
            pixels[index + 3] = 0xFF;
        }
    }
    (pixels, width, height)
}

/// R1 게이트 검증용 초기 상태 주입 (임시 — wine에 합성 키 입력이 전달되지 않아
/// 상태별 인스턴스를 띄워 캡처한다. R3 입력 구현 후 제거)
/// 예: RIV_R1_STATE="zoom=0.45;rotate=1;mirror;tier=0;pan=100:50"
fn apply_debug_initial_state(application: &mut Application) {
    let Ok(state) = std::env::var("RIV_R1_STATE") else {
        return;
    };
    for token in state.split(';') {
        let (name, value) = match token.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (token, None),
        };
        let transform = &mut application.view_transform;
        match (name, value) {
            ("rotate", Some(value)) => {
                transform.rotation_quadrant = value.parse::<u32>().unwrap_or(0) % 4;
            }
            ("mirror", None) => transform.mirrored = true,
            ("flip", None) => transform.flipped = true,
            ("zoom", Some(value)) => {
                transform.scale =
                    value.parse::<f32>().unwrap_or(1.0) * transform.device_pixel_ratio;
                transform.fit_tracking = false;
            }
            ("pan", Some(value)) => {
                if let Some((x, y)) = value.split_once(':') {
                    transform.pan_offset_x = x.parse().unwrap_or(0.0);
                    transform.pan_offset_y = y.parse().unwrap_or(0.0);
                }
            }
            ("tier", Some(value)) => {
                application.scaling_tier = value.parse().unwrap_or(1);
            }
            _ => {}
        }
    }
}

fn toggle_fullscreen(application: &mut Application, window: HWND) {
    unsafe {
        if let Some((placement, style)) = application.fullscreen_restore.take() {
            SetWindowLongPtrW(window, GWL_STYLE, style.0 as isize);
            let _ = SetWindowPlacement(window, &placement);
            let _ = SetWindowPos(
                window,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        } else {
            let mut placement = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            let _ = GetWindowPlacement(window, &mut placement);
            let style = WINDOW_STYLE(GetWindowLongPtrW(window, GWL_STYLE) as u32);
            application.fullscreen_restore = Some((placement, style));

            let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
            let mut monitor_info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(monitor, &mut monitor_info);
            let bounds = monitor_info.rcMonitor;

            SetWindowLongPtrW(window, GWL_STYLE, ((WS_POPUP | WS_VISIBLE).0) as isize);
            let _ = SetWindowPos(
                window,
                Some(HWND_TOP),
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_FRAMECHANGED,
            );
        }
    }
}

/// 임시 디버그 입력 (R1 게이트 검증용 — R3에서 bindings 디스패치로 대체)
fn handle_debug_key(application: &mut Application, window: HWND, key: u16) {
    let (width, height) = client_size(window);
    let viewport = Size {
        width: width as f32,
        height: height as f32,
    };
    let image = application.image_size();
    let transform = &mut application.view_transform;
    match key {
        key if key == VK_OEM_PLUS.0 || key == VK_ADD.0 => {
            transform.zoom(1.25, None, viewport, image);
        }
        key if key == VK_OEM_MINUS.0 || key == VK_SUBTRACT.0 => {
            transform.zoom(0.8, None, viewport, image);
        }
        key if key == VK_LEFT.0 => transform.pan_by(64.0, 0.0, viewport, image),
        key if key == VK_RIGHT.0 => transform.pan_by(-64.0, 0.0, viewport, image),
        key if key == VK_UP.0 => transform.pan_by(0.0, 64.0, viewport, image),
        key if key == VK_DOWN.0 => transform.pan_by(0.0, -64.0, viewport, image),
        key if key == VK_BACK.0 => transform.toggle_zoom(viewport, image),
        key if key == u16::from(b'R') => transform.rotate(1, viewport, image),
        key if key == u16::from(b'E') => transform.rotate(-1, viewport, image),
        key if key == u16::from(b'M') => transform.mirror(),
        key if key == u16::from(b'V') => transform.flip(),
        key if (u16::from(b'1')..=u16::from(b'4')).contains(&key) => {
            application.scaling_tier = u32::from(key - u16::from(b'1'));
        }
        key if key == VK_F11.0 => {
            toggle_fullscreen(application, window);
        }
        _ => return,
    }
    application.render(window);
}

fn main() -> Result<()> {
    let instance = unsafe { GetModuleHandleW(None)? };
    let class_name = w!("riv");

    let application_icon = unsafe { LoadIconW(Some(instance.into()), APPLICATION_ICON_ID)? };
    let window_class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_procedure),
        hInstance: instance.into(),
        hIcon: application_icon,
        hIconSm: application_icon,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        hbrBackground: unsafe { GetSysColorBrush(COLOR_WINDOW) },
        lpszClassName: class_name,
        ..Default::default()
    };
    let class_atom = unsafe { RegisterClassExW(&window_class) };
    assert!(class_atom != 0, "RegisterClassExW failed");

    let window = unsafe {
        CreateWindowExW(
            Default::default(),
            class_name,
            w!("riv"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };

    let mut application = Box::new(Application::new(window)?);
    apply_debug_initial_state(&mut application);
    let debug_fullscreen = std::env::var("RIV_R1_STATE")
        .is_ok_and(|state| state.split(';').any(|token| token == "fullscreen"));
    unsafe {
        SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(application) as isize);
    }
    if let Some(application) = unsafe { application_from_window(window) } {
        if debug_fullscreen {
            toggle_fullscreen(application, window);
        }
        application.render(window);
    }
    let _ = unsafe { ShowWindow(window, SW_SHOW) };

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        let _ = unsafe { TranslateMessage(&message) };
        unsafe { DispatchMessageW(&message) };
    }
    Ok(())
}

/// GWLP_USERDATA에 실린 Application 포인터 복원
unsafe fn application_from_window(window: HWND) -> Option<&'static mut Application> {
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut Application;
    unsafe { pointer.as_mut() }
}

extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // 동기 리사이즈 → 즉시 재렌더 (무플래시 요구, SPEC §6.2·§11)
        WM_SIZE => {
            if let Some(application) = unsafe { application_from_window(window) } {
                let width = (lparam.0 & 0xFFFF) as u32;
                let height = ((lparam.0 >> 16) & 0xFFFF) as u32;
                if width > 0 && height > 0 {
                    if application.renderer.resize(width, height).is_err() {
                        let _ = application.rebuild_renderer(window);
                    }
                    application.render(window);
                }
            }
            LRESULT(0)
        }
        // 렌더는 온디맨드 — WM_PAINT는 ValidateRect만 (PORTING_PLAN §3 렌더러 세부)
        WM_PAINT => {
            let _ = unsafe { ValidateRect(Some(window), None) };
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if let Some(application) = unsafe { application_from_window(window) } {
                handle_debug_key(application, window, wparam.0 as u16);
            }
            LRESULT(0)
        }
        // 임시 디버그: 휠 줌(커서 앵커) — R3에서 bindings 디스패치로 대체
        WM_MOUSEWHEEL => {
            if let Some(application) = unsafe { application_from_window(window) } {
                let wheel_delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let mut cursor = windows::Win32::Foundation::POINT {
                    x: (lparam.0 & 0xFFFF) as u16 as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
                };
                let _ = unsafe { ScreenToClient(window, &mut cursor) };
                let (width, height) = client_size(window);
                let viewport = Size {
                    width: width as f32,
                    height: height as f32,
                };
                let image = application.image_size();
                let cursor_from_center = (
                    cursor.x as f32 - viewport.width / 2.0,
                    cursor.y as f32 - viewport.height / 2.0,
                );
                let factor = if wheel_delta > 0 { 1.25 } else { 0.8 };
                application
                    .view_transform
                    .zoom(factor, Some(cursor_from_center), viewport, image);
                application.render(window);
            }
            LRESULT(0)
        }
        // Per-Monitor V2: 제안 사각형 적용 + 배율 기준 갱신
        WM_DPICHANGED => {
            if let Some(application) = unsafe { application_from_window(window) } {
                application.view_transform.device_pixel_ratio = (wparam.0 & 0xFFFF) as f32 / 96.0;
            }
            let suggested_bounds = unsafe { &*(lparam.0 as *const RECT) };
            let _ = unsafe {
                SetWindowPos(
                    window,
                    None,
                    suggested_bounds.left,
                    suggested_bounds.top,
                    suggested_bounds.right - suggested_bounds.left,
                    suggested_bounds.bottom - suggested_bounds.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let pointer =
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) } as *mut Application;
            if !pointer.is_null() {
                drop(unsafe { Box::from_raw(pointer) });
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}
