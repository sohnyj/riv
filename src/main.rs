#![windows_subsystem = "windows"]

mod actions;
mod bindings;
mod image;
mod settings;
mod view;
mod window;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use actions::{Action, ActivationGate};
use bindings::{
    Bindings, MODIFIER_ALT, MODIFIER_CONTROL, MODIFIER_META, MODIFIER_SHIFT, MouseBase,
};
use image::core::{
    CoreOptions, DecodeCompletion, ImageCore, NavigationCommand, SortMode, WM_APP_DECODE_COMPLETE,
};
use image::decode::DecodedImage;
use settings::{Options, SettingsFile};
use view::renderer::Renderer;
use view::transform::{FitMode, Size, ViewTransform};
use window::context_menu::{self, MenuState};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::D2D1_INTERPOLATION_MODE;
use windows::Win32::Graphics::Direct2D::{
    D2D1_INTERPOLATION_MODE_CUBIC, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
};
use windows::Win32::Graphics::Gdi::{
    COLOR_WINDOW, GetMonitorInfoW, GetSysColor, GetSysColorBrush, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, ScreenToClient, ValidateRect,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU,
    VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, GWL_STYLE, GWLP_USERDATA, GetClientRect, GetCursorPos, GetMessageW,
    GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, HCURSOR, HTCAPTION, HWND_TOP, IDC_ARROW,
    IDC_SIZEALL, IsZoomed, KillTimer, LoadCursorW, LoadIconW, MSG, PostMessageW, PostQuitMessage,
    RegisterClassExW, SW_SHOW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SendMessageW, SetCursor, SetTimer, SetWindowLongPtrW, SetWindowPlacement,
    SetWindowPos, ShowWindow, TranslateMessage, WINDOW_STYLE, WINDOWPLACEMENT, WM_ACTIVATEAPP,
    WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONDBLCLK,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOVE,
    WM_NCDESTROY, WM_NCLBUTTONDOWN, WM_PAINT, WM_SETCURSOR, WM_SIZE, WM_SYSKEYDOWN, WM_TIMER,
    WM_XBUTTONDOWN, WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, Result, w};

// res/riv.rc의 아이콘 리소스 ID (MAKEINTRESOURCE — 정수 1을 포인터 슬롯에 싣는다)
const APPLICATION_ICON_ID: PCWSTR = PCWSTR(std::ptr::without_provenance(1));

/// 무인자 실행 시 다음 이벤트 루프 턴에 빈 창 표시 (SPEC §6.1 지연 첫 표시)
const WM_APP_SHOW_WINDOW: u32 = WM_APP + 2;

/// R3 게이트 검증용 액션 스크립트 타이머 (임시 — wine 합성 키 입력 불가, R4 검증 후 제거)
const ACTION_SCRIPT_TIMER: usize = 1;

/// 키보드/메뉴 팬 스텝 (디바이스 픽셀)
const PAN_STEP: f32 = 64.0;

struct Application {
    renderer: Renderer,
    view_transform: ViewTransform,
    image_core: ImageCore,
    /// 현재 표시 이미지 — 디바이스 로스트 재구축 시 재업로드용
    display: Option<Arc<DecodedImage>>,
    settings: SettingsFile,
    bindings: Bindings,
    /// 창 단위 휘발성 토글 — 파일 이동·전체화면 전환에도 절대 배율 유지 (SPEC §3.2)
    preserve_zoom: bool,
    /// 전체화면 진입 전 창 상태 (DWM 보정은 R7)
    fullscreen_restore: Option<(WINDOWPLACEMENT, WINDOW_STYLE)>,
    /// 백버퍼 포맷 재평가용 — 모니터 이동 감지 (SPEC §3.1 비트 심도 매칭)
    current_monitor: HMONITOR,
    /// 팬 드래그 중 마지막 커서 위치 (클라이언트 좌표) (SPEC §5.4)
    pan_drag_position: Option<(i32, i32)>,
    pan_cursor: HCURSOR,
    /// 휠 노치 누적 — 프랙셔널 줌 외 액션은 노치당 1회 (SPEC §5.3)
    wheel_notch_accumulator: i32,
    /// 지연 첫 표시 (SPEC §6.1) — 첫 이미지(또는 실패) 후 show
    window_shown: bool,
    /// R3 게이트 검증용 액션 스크립트 (임시)
    action_script: VecDeque<Action>,
}

impl Application {
    fn new(window: HWND) -> Result<Self> {
        let (width, height) = client_size(window);
        let renderer = Renderer::new(window, width.max(1), height.max(1))?;
        let device_pixel_ratio = unsafe { GetDpiForWindow(window) } as f32 / 96.0;
        let settings = SettingsFile::load();
        let bindings =
            Bindings::from_settings(settings.keyboard_bindings(), settings.mouse_bindings());
        let mut view_transform = ViewTransform::new(device_pixel_ratio);
        view_transform.fit_mode = FitMode::from_setting(settings.options.fit_mode);
        let mut application = Self {
            renderer,
            view_transform,
            image_core: ImageCore::new(window, core_options(&settings.options)),
            display: None,
            settings,
            bindings,
            preserve_zoom: false,
            fullscreen_restore: None,
            current_monitor: unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) },
            pan_drag_position: None,
            pan_cursor: unsafe { LoadCursorW(None, IDC_SIZEALL)? },
            wheel_notch_accumulator: 0,
            window_shown: false,
            action_script: parse_action_script(),
        };
        // 실행 인자 = 열 파일 경로 하나 (SPEC §6.5 — CLI 옵션 없음)
        if let Some(argument) = std::env::args_os().nth(1) {
            application.image_core.load_path(Path::new(&argument));
        }
        Ok(application)
    }

    fn image_size(&self) -> Size {
        let (width, height) = self
            .display
            .as_ref()
            .map_or((1, 1), |image| (image.width, image.height));
        Size {
            width: width as f32,
            height: height as f32,
        }
    }

    fn viewport(&self, window: HWND) -> Size {
        let (width, height) = client_size(window);
        Size {
            width: width as f32,
            height: height as f32,
        }
    }

    /// Scaling 설정 → D2D 보간 모드 (SPEC §3.3)
    fn interpolation_mode(&self) -> D2D1_INTERPOLATION_MODE {
        match self.settings.options.scaling_filter {
            0 => D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            2 => D2D1_INTERPOLATION_MODE_CUBIC,
            3 => D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
            _ => D2D1_INTERPOLATION_MODE_LINEAR,
        }
    }

    /// 배경색 — 설정 색 또는 시스템 창 배경색 (SPEC §3.1·§8.2)
    fn background_color(&self) -> D2D1_COLOR_F {
        let (red, green, blue) = if self.settings.options.background_color_enabled {
            self.settings.options.background_color
        } else {
            let colorref = unsafe { GetSysColor(COLOR_WINDOW) };
            (
                (colorref & 0xFF) as u8,
                ((colorref >> 8) & 0xFF) as u8,
                ((colorref >> 16) & 0xFF) as u8,
            )
        };
        D2D1_COLOR_F {
            r: f32::from(red) / 255.0,
            g: f32::from(green) / 255.0,
            b: f32::from(blue) / 255.0,
            a: 1.0,
        }
    }

    /// 지연 첫 표시 (SPEC §6.1) — 로드 실패해도 반드시 표시
    fn ensure_window_shown(&mut self, window: HWND) {
        if self.window_shown {
            return;
        }
        self.window_shown = true;
        let _ = unsafe { ShowWindow(window, SW_SHOW) };
        if !self.action_script.is_empty() {
            unsafe { SetTimer(Some(window), ACTION_SCRIPT_TIMER, 700, None) };
        }
    }

    /// 새 현재 이미지 반영 — 회전·팬 리셋, Preserve Zoom이면 절대 배율 유지 (SPEC §3.2·§4.1)
    fn apply_current_image(&mut self, window: HWND) {
        let Some(current) = &self.image_core.current else {
            return;
        };
        let image = current.image.clone();
        let frame = &image.frames[0];
        let upload = self.renderer.set_image(
            &frame.pixels,
            image.pixel_width,
            image.pixel_height,
            (image.width, image.height),
        );
        self.display = Some(image);
        let transform = &mut self.view_transform;
        transform.rotation_quadrant = 0;
        transform.mirrored = false;
        transform.flipped = false;
        transform.pan_offset_x = 0.0;
        transform.pan_offset_y = 0.0;
        // Preserve Zoom: 배율 유지·fit 재적용 안 함(팬만 리셋+클램프), 아니면 fit
        transform.fit_tracking = !self.preserve_zoom;
        if upload.is_err() {
            // 업로드 실패(디바이스 로스트 등) — 재구축 경로가 display에서 재업로드
            let _ = self.rebuild_renderer(window);
        }
        self.render(window);
    }

    /// 디바이스 로스트·모니터 이동 시 전체 재구축 — 백버퍼 포맷도 재감지 (SPEC §3.1·§3.4)
    fn rebuild_renderer(&mut self, window: HWND) -> Result<()> {
        let (width, height) = client_size(window);
        self.current_monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
        self.renderer = Renderer::new(window, width.max(1), height.max(1))?;
        if let Some(image) = &self.display {
            self.renderer.set_image(
                &image.frames[0].pixels,
                image.pixel_width,
                image.pixel_height,
                (image.width, image.height),
            )?;
        }
        Ok(())
    }

    fn render(&mut self, window: HWND) {
        let (width, height) = client_size(window);
        if width == 0 || height == 0 {
            return;
        }
        let viewport = self.viewport(window);
        let image = self.image_size();
        self.view_transform.synchronize(viewport, image);
        let matrix = self.view_transform.matrix(viewport, image);
        let interpolation = self.interpolation_mode();
        let background = self.background_color();
        if self
            .renderer
            .render(matrix, interpolation, background)
            .is_err()
        {
            // 디바이스 로스트 — 재구축 후 1회 재시도
            if self.rebuild_renderer(window).is_ok() {
                let _ = self.renderer.render(matrix, interpolation, background);
            }
        }
    }

    /// 설정 변경 브로드캐스트 (SPEC §8.1~8.2, §2 핵심 계약 — 현재 줌/팬 불변)
    fn apply_options(&mut self, window: HWND) {
        self.bindings = Bindings::from_settings(
            self.settings.keyboard_bindings(),
            self.settings.mouse_bindings(),
        );
        self.view_transform.fit_mode = FitMode::from_setting(self.settings.options.fit_mode);
        self.image_core
            .update_options(core_options(&self.settings.options));
        self.render(window);
    }

    /// 활성화 게이트 (SPEC §5.1)
    fn gate_satisfied(&self, gate: ActivationGate) -> bool {
        match gate {
            ActivationGate::Window => true,
            ActivationGate::Image => self.image_core.current.is_some(),
            ActivationGate::Animation => self
                .image_core
                .current
                .as_ref()
                .is_some_and(|current| current.image.frames.len() > 1),
            ActivationGate::Folder => self.image_core.has_folder_entries(),
        }
    }

    /// 곱셈 줌 공용 경로 — 커서가 뷰 위면 커서 앵커 (SPEC §3.2 커서 줌)
    fn zoom_by(&mut self, window: HWND, factor: f32) {
        let anchor = if self.settings.options.cursor_zoom {
            cursor_from_center(window)
        } else {
            None
        };
        let viewport = self.viewport(window);
        let image = self.image_size();
        self.view_transform.zoom(factor, anchor, viewport, image);
        self.render(window);
    }

    /// 휠 줌 (SPEC §5.3) — 프랙셔널이면 스텝 × (델타/120), 아니면 노치 단위
    fn wheel_zoom(&mut self, window: HWND, wheel_delta: i16) {
        let step = 1.0 + self.settings.options.scale_factor_percent as f32 / 100.0;
        let exponent = if self.settings.options.fractional_zoom {
            f32::from(wheel_delta) / 120.0
        } else {
            let notches = self.accumulate_wheel_notches(wheel_delta);
            if notches == 0 {
                return;
            }
            notches as f32
        };
        self.zoom_by(window, step.powf(exponent));
    }

    fn accumulate_wheel_notches(&mut self, wheel_delta: i16) -> i32 {
        self.wheel_notch_accumulator += i32::from(wheel_delta);
        let notches = self.wheel_notch_accumulator / 120;
        self.wheel_notch_accumulator %= 120;
        notches
    }

    fn pan_by(&mut self, window: HWND, delta_x: f32, delta_y: f32) {
        let viewport = self.viewport(window);
        let image = self.image_size();
        self.view_transform
            .pan_by(delta_x, delta_y, viewport, image);
        self.render(window);
    }
}

fn core_options(options: &Options) -> CoreOptions {
    CoreOptions {
        sort_mode: SortMode::from_setting(options.sort_mode),
        sort_descending: options.sort_descending,
        preloading_mode: options.preloading_mode as usize,
        loop_folders_enabled: options.loop_folders_enabled,
        skip_hidden: options.skip_hidden,
        allow_mime_content_detection: options.allow_mime_content_detection,
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

/// 커서가 뷰(클라이언트) 위에 있으면 중심 기준 오프셋 (SPEC §3.2 커서 앵커)
fn cursor_from_center(window: HWND) -> Option<(f32, f32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    let _ = unsafe { ScreenToClient(window, &mut point) };
    let (width, height) = client_size(window);
    if point.x < 0 || point.y < 0 || point.x >= width as i32 || point.y >= height as i32 {
        return None;
    }
    Some((
        point.x as f32 - width as f32 / 2.0,
        point.y as f32 - height as f32 / 2.0,
    ))
}

/// 현재 눌린 수정자 (바인딩 인코딩과 동일 비트)
fn current_modifiers() -> u8 {
    let pressed = |key: VIRTUAL_KEY| unsafe { GetKeyState(i32::from(key.0)) } < 0;
    let mut modifiers = 0u8;
    if pressed(VK_CONTROL) {
        modifiers |= MODIFIER_CONTROL;
    }
    if pressed(VK_SHIFT) {
        modifiers |= MODIFIER_SHIFT;
    }
    if pressed(VK_MENU) {
        modifiers |= MODIFIER_ALT;
    }
    if pressed(VK_LWIN) || pressed(VK_RWIN) {
        modifiers |= MODIFIER_META;
    }
    modifiers
}

/// R3 게이트 검증용 액션 스크립트 (임시 — wine 합성 키 불가, 액션 계층을 구동.
/// 키 디코드 계층은 실기 확인). 예: RIV_R3_ACTIONS="nextfile;zoomin;rotateright"
fn parse_action_script() -> VecDeque<Action> {
    std::env::var("RIV_R3_ACTIONS").map_or_else(
        |_| VecDeque::new(),
        |script| {
            script
                .split(';')
                .filter_map(|token| Action::from_name(token.trim()))
                .collect()
        },
    )
}

fn execute_navigation(application: &mut Application, window: HWND, command: NavigationCommand) {
    // true = 캐시 히트로 동기 표시 변경 — 비동기 완료는 WM_APP_DECODE_COMPLETE에서 반영
    if application.image_core.navigate(command) {
        application.apply_current_image(window);
    }
}

/// 단일 디스패치 지점 (SPEC §5.1, §2 핵심 계약) — 모든 입력·메뉴가 여기로 수렴
fn dispatch_action(application: &mut Application, window: HWND, action: Action) {
    if !application.gate_satisfied(action.gate()) {
        return;
    }
    let zoom_step = 1.0 + application.settings.options.scale_factor_percent as f32 / 100.0;
    match action {
        Action::Quit | Action::CloseWindow => {
            let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        }
        Action::FirstFile => execute_navigation(application, window, NavigationCommand::First),
        Action::PreviousFile => {
            execute_navigation(application, window, NavigationCommand::Previous);
        }
        Action::NextFile => execute_navigation(application, window, NavigationCommand::Next),
        Action::LastFile => execute_navigation(application, window, NavigationCommand::Last),
        Action::ReloadFile => {
            if application.image_core.reload_current() {
                application.apply_current_image(window);
            }
        }
        Action::ZoomIn => application.zoom_by(window, zoom_step),
        Action::ZoomOut => application.zoom_by(window, 1.0 / zoom_step),
        Action::ResetZoom => {
            let viewport = application.viewport(window);
            let image = application.image_size();
            application.view_transform.toggle_zoom(viewport, image);
            application.render(window);
        }
        Action::PreserveZoom => {
            application.preserve_zoom = !application.preserve_zoom;
            // 상태 필 오버레이 표시는 R4
        }
        Action::PanUp => application.pan_by(window, 0.0, PAN_STEP),
        Action::PanDown => application.pan_by(window, 0.0, -PAN_STEP),
        Action::PanLeft => application.pan_by(window, PAN_STEP, 0.0),
        Action::PanRight => application.pan_by(window, -PAN_STEP, 0.0),
        Action::RotateRight | Action::RotateLeft => {
            let step = if action == Action::RotateRight { 1 } else { -1 };
            let viewport = application.viewport(window);
            let image = application.image_size();
            application.view_transform.rotate(step, viewport, image);
            application.render(window);
        }
        Action::Mirror => {
            application.view_transform.mirror();
            application.render(window);
        }
        Action::Flip => {
            application.view_transform.flip();
            application.render(window);
        }
        Action::Fullscreen => {
            toggle_fullscreen(application, window);
            application.render(window);
        }
        // R4: 셸 통합·오버레이·최근 파일·슬라이드쇼
        Action::Open
        | Action::OpenWith
        | Action::OpenWithOther
        | Action::OpenContainingFolder
        | Action::ShowFileInfo
        | Action::Rename
        | Action::Delete
        | Action::DeletePermanent
        | Action::Copy
        | Action::Paste
        | Action::Recent(_)
        | Action::ClearRecents
        | Action::Slideshow => {}
        // R5: 애니메이션 스케줄러
        Action::Pause
        | Action::NextFrame
        | Action::DecreaseSpeed
        | Action::ResetSpeed
        | Action::IncreaseSpeed => {}
        // R6: 옵션 다이얼로그 / R7: 멀티윈도우
        Action::Options | Action::NewWindow | Action::CloseAllWindows => {}
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

/// 키 입력 → 바인딩 조회 → 디스패치 (SPEC §5.2). 반환 = 처리 여부.
fn handle_key(application: &mut Application, window: HWND, virtual_key: u16) -> bool {
    if [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
        .iter()
        .any(|modifier| modifier.0 == virtual_key)
    {
        return false;
    }
    let modifiers = current_modifiers();
    if let Some(action) = application.bindings.lookup_key(modifiers, virtual_key) {
        dispatch_action(application, window, action);
        return true;
    }
    // Escape 특례 — 어떤 액션에도 안 묶였을 때만 전체화면 나가기 전용 (SPEC §5.2)
    if virtual_key == VK_ESCAPE.0
        && application.bindings.escape_is_unbound()
        && application.fullscreen_restore.is_some()
    {
        toggle_fullscreen(application, window);
        application.render(window);
        return true;
    }
    false
}

/// 휠 → 바인딩 (SPEC §5.3) — zoom/pan 계열은 델타 직접 소비, 그 외 노치당 1회
fn handle_wheel(application: &mut Application, window: HWND, wheel_delta: i16) {
    let base = if wheel_delta > 0 {
        MouseBase::WheelUp
    } else {
        MouseBase::WheelDown
    };
    let Some(action) = application
        .bindings
        .lookup_mouse(current_modifiers(), false, base)
    else {
        return;
    };
    if !application.gate_satisfied(action.gate()) {
        return;
    }
    let pan_amount = f32::from(wheel_delta.abs()) / 2.0;
    match action {
        Action::ZoomIn | Action::ZoomOut => application.wheel_zoom(window, wheel_delta),
        Action::PanUp => application.pan_by(window, 0.0, pan_amount),
        Action::PanDown => application.pan_by(window, 0.0, -pan_amount),
        Action::PanLeft => application.pan_by(window, pan_amount, 0.0),
        Action::PanRight => application.pan_by(window, -pan_amount, 0.0),
        action => {
            let notches = application.accumulate_wheel_notches(wheel_delta);
            for _ in 0..notches.abs() {
                dispatch_action(application, window, action);
            }
        }
    }
}

fn main() -> Result<()> {
    // UI 스레드 = STA, 디코드 워커 = MTA (PORTING_PLAN §3 매핑)
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()?;
    let instance = unsafe { GetModuleHandleW(None)? };
    let class_name = w!("riv");

    let application_icon = unsafe { LoadIconW(Some(instance.into()), APPLICATION_ICON_ID)? };
    let window_class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
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

    let application = Box::new(Application::new(window)?);
    // 지연 첫 표시 (SPEC §6.1): 로드 진행 중이면 완료(또는 실패) 시점에,
    // 아니면 다음 이벤트 루프 턴에 표시
    let load_pending = application.image_core.is_load_pending();
    unsafe {
        SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(application) as isize);
    }
    if let Some(application) = unsafe { application_from_window(window) } {
        application.render(window);
        if !load_pending {
            let _ = unsafe { PostMessageW(Some(window), WM_APP_SHOW_WINDOW, WPARAM(0), LPARAM(0)) };
        }
    }

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
        // 디코드 워커 완료 통지 — lparam = Box<DecodeCompletion> (PORTING_PLAN §2)
        WM_APP_DECODE_COMPLETE => {
            let completion = unsafe { Box::from_raw(lparam.0 as *mut DecodeCompletion) };
            if let Some(application) = unsafe { application_from_window(window) }
                && application.image_core.on_decode_complete(*completion)
            {
                application.apply_current_image(window);
                application.ensure_window_shown(window);
            }
            LRESULT(0)
        }
        // 무인자 실행 — 다음 이벤트 루프 턴에 빈 창 표시 (SPEC §6.1)
        WM_APP_SHOW_WINDOW => {
            if let Some(application) = unsafe { application_from_window(window) } {
                application.ensure_window_shown(window);
            }
            LRESULT(0)
        }
        // R3 검증 스크립트 스텝 (임시)
        WM_TIMER if wparam.0 == ACTION_SCRIPT_TIMER => {
            if let Some(application) = unsafe { application_from_window(window) } {
                match application.action_script.pop_front() {
                    Some(action) => dispatch_action(application, window, action),
                    None => {
                        let _ = unsafe { KillTimer(Some(window), ACTION_SCRIPT_TIMER) };
                    }
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let handled = unsafe { application_from_window(window) }
                .is_some_and(|application| handle_key(application, window, wparam.0 as u16));
            if !handled && message == WM_SYSKEYDOWN {
                // 시스템 키 기본 처리(Alt 메뉴 등) 유지
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            } else {
                LRESULT(0)
            }
        }
        WM_MOUSEWHEEL => {
            if let Some(application) = unsafe { application_from_window(window) } {
                let wheel_delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                handle_wheel(application, window, wheel_delta);
            }
            LRESULT(0)
        }
        // 좌클릭 = 팬 드래그 예약, Ctrl+좌드래그 = 창 이동 (SPEC §5.3~5.4)
        WM_LBUTTONDOWN => {
            if let Some(application) = unsafe { application_from_window(window) } {
                let move_window = current_modifiers() == MODIFIER_CONTROL
                    && application.settings.options.control_drag_window
                    && application.fullscreen_restore.is_none()
                    && !unsafe { IsZoomed(window) }.as_bool();
                if move_window {
                    // 시스템 이동 우선 (SPEC §5.4)
                    let _ = unsafe { ReleaseCapture() };
                    unsafe {
                        SendMessageW(
                            window,
                            WM_NCLBUTTONDOWN,
                            Some(WPARAM(HTCAPTION as usize)),
                            Some(LPARAM(0)),
                        )
                    };
                } else {
                    unsafe { SetCapture(window) };
                    unsafe { SetCursor(Some(application.pan_cursor)) };
                    application.pan_drag_position = Some((
                        (lparam.0 & 0xFFFF) as u16 as i16 as i32,
                        ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
                    ));
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(application) = unsafe { application_from_window(window) }
                && let Some((last_x, last_y)) = application.pan_drag_position
            {
                let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                application.pan_drag_position = Some((x, y));
                application.pan_by(window, (x - last_x) as f32, (y - last_y) as f32);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(application) = unsafe { application_from_window(window) }
                && application.pan_drag_position.take().is_some()
            {
                let _ = unsafe { ReleaseCapture() };
            }
            LRESULT(0)
        }
        // 팬 드래그 중 클로즈드핸드 대체 커서 유지 (커서 자산은 R7)
        WM_SETCURSOR => {
            if let Some(application) = unsafe { application_from_window(window) }
                && application.pan_drag_position.is_some()
            {
                unsafe { SetCursor(Some(application.pan_cursor)) };
                LRESULT(1)
            } else {
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }
        }
        WM_LBUTTONDBLCLK => {
            if let Some(application) = unsafe { application_from_window(window) }
                && let Some(action) =
                    application
                        .bindings
                        .lookup_mouse(current_modifiers(), true, MouseBase::Left)
            {
                dispatch_action(application, window, action);
            }
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            if let Some(application) = unsafe { application_from_window(window) }
                && let Some(action) =
                    application
                        .bindings
                        .lookup_mouse(current_modifiers(), false, MouseBase::Middle)
            {
                dispatch_action(application, window, action);
            }
            LRESULT(0)
        }
        WM_XBUTTONDOWN => {
            if let Some(application) = unsafe { application_from_window(window) } {
                // HIWORD(wparam): 1=XBUTTON1(Back), 2=XBUTTON2(Forward)
                let base = if (wparam.0 >> 16) & 0xFFFF == 1 {
                    MouseBase::Back
                } else {
                    MouseBase::Forward
                };
                if let Some(action) =
                    application
                        .bindings
                        .lookup_mouse(current_modifiers(), false, base)
                {
                    dispatch_action(application, window, action);
                }
            }
            LRESULT(1) // 처리 표시 (기본 앱 커맨드 변환 방지)
        }
        // 우클릭 예약 — 컨텍스트 메뉴 전용 (SPEC §5.3, §6.1)
        WM_CONTEXTMENU => {
            if let Some(application) = unsafe { application_from_window(window) } {
                let mut x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let mut y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                if x == -1 && y == -1 {
                    // 키보드 메뉴 키 — 창 중앙
                    let mut bounds = RECT::default();
                    let _ = unsafe { GetWindowRect(window, &mut bounds) };
                    x = (bounds.left + bounds.right) / 2;
                    y = (bounds.top + bounds.bottom) / 2;
                }
                let state = MenuState {
                    has_image: application.image_core.current.is_some(),
                    has_folder: application.image_core.has_folder_entries(),
                    has_animation: application
                        .image_core
                        .current
                        .as_ref()
                        .is_some_and(|current| current.image.frames.len() > 1),
                    preserve_zoom: application.preserve_zoom,
                    fullscreen: application.fullscreen_restore.is_some(),
                };
                if let Some(action) = context_menu::show(window, state, x, y) {
                    dispatch_action(application, window, action);
                }
            }
            LRESULT(0)
        }
        // 앱 재활성화 — 설정 파일 재로드·브로드캐스트 (SPEC §8.1)
        WM_ACTIVATEAPP => {
            if wparam.0 != 0
                && let Some(application) = unsafe { application_from_window(window) }
                && application.settings.reload()
            {
                application.apply_options(window);
            }
            LRESULT(0)
        }
        // 모니터 이동 감지 → 백버퍼 비트 심도 재평가 (SPEC §3.1)
        WM_MOVE => {
            if let Some(application) = unsafe { application_from_window(window) } {
                let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
                if monitor != application.current_monitor
                    && application.rebuild_renderer(window).is_ok()
                {
                    application.render(window);
                }
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
