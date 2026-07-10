#![windows_subsystem = "windows"]

mod actions;
mod bindings;
mod dialogs;
mod image;
mod settings;
mod shell;
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
use shell::clipboard::{self, BakedOrientation};
use shell::drag_drop::{self, WM_APP_DROP_PATH};
use shell::open_with::{self, OpenWithList, WM_APP_OPEN_WITH_LIST};
use shell::{file_ops, open_dialog};
use view::renderer::Renderer;
use view::transform::{FitMode, Size, ViewTransform};
use window::context_menu::{self, MenuSelection, MenuState};
use window::overlay::{self, Overlay, OverlayContent};
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
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{IDropTarget, OleInitialize, RevokeDragDrop};
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
/// 줌 필 1초 자동 숨김 (SPEC §3.6)
const ZOOM_PILL_TIMER: usize = 2;
/// 슬라이드쇼 간격 (SPEC §6.3)
const SLIDESHOW_TIMER: usize = 3;
/// 최근 파일 500ms 디바운스 저장 (SPEC §6.4)
const RECENTS_SAVE_TIMER: usize = 4;
/// Open With 목록 채우기 — 파일 변경 250ms 디바운스 (SPEC §6.4)
const OPEN_WITH_TIMER: usize = 5;

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
    overlay: Overlay,
    /// Show File Info 토글 (SPEC §3.6 정보 오버레이)
    show_file_info: bool,
    /// 줌 필 텍스트 — 1초 자동 숨김 (SPEC §3.6)
    zoom_pill_text: Option<String>,
    /// 슬라이드쇼 상태 (SPEC §6.3)
    slideshow_active: bool,
    /// 드롭 타깃 — 창 수명 동안 유지 (SPEC §5.4)
    drop_target: Option<IDropTarget>,
    /// Open With 핸들러 목록 — 백그라운드 열거 결과, 파일 전환 시 폐기 (SPEC §6.4)
    open_with_list: Option<Box<OpenWithList>>,
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
            overlay: Overlay::new()?,
            show_file_info: false,
            zoom_pill_text: None,
            slideshow_active: false,
            drop_target: None,
            open_with_list: None,
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
        // 최근 파일 수집 — 500ms 디바운스 저장 (SPEC §6.4)
        let recent_path = self
            .image_core
            .current
            .as_ref()
            .map(|current| current.path.clone());
        if let Some(path) = recent_path
            && self.settings.add_recent_file(&path)
        {
            unsafe { SetTimer(Some(window), RECENTS_SAVE_TIMER, 500, None) };
        }
        // Open With 목록 갱신 — 파일 변경 250ms 디바운스 (SPEC §6.4)
        self.open_with_list = None;
        unsafe { SetTimer(Some(window), OPEN_WITH_TIMER, 250, None) };
        self.render(window);
    }

    /// 디코드 실패 반영 — 이미지 제거 + 에러 텍스트 (SPEC §3.6·§4.2)
    fn apply_load_error(&mut self, window: HWND) {
        self.display = None;
        self.renderer.clear_image();
        self.render(window);
    }

    /// 줌 필 표시 + 1초 자동 숨김 타이머 (SPEC §3.6)
    fn show_zoom_pill(&mut self, window: HWND, text: String) {
        self.zoom_pill_text = Some(text);
        unsafe { SetTimer(Some(window), ZOOM_PILL_TIMER, 1000, None) };
    }

    /// 슬라이드쇼 토글 (SPEC §6.3) — 상태 필 "Slideshow: Start/Stop" (SPEC §3.6)
    fn toggle_slideshow(&mut self, window: HWND) {
        if self.slideshow_active {
            self.cancel_slideshow(window);
        } else {
            let interval =
                (self.settings.options.slideshow_timer_seconds * 1000.0).max(100.0) as u32;
            unsafe { SetTimer(Some(window), SLIDESHOW_TIMER, interval, None) };
            self.slideshow_active = true;
            self.show_zoom_pill(window, "Slideshow: Start".to_string());
            self.render(window);
        }
    }

    /// 수동 파일 로드·드롭·폴더 끝(루프 off) 시 자동 취소 (SPEC §6.3) —
    /// 자동 취소도 상태 필로 알림
    fn cancel_slideshow(&mut self, window: HWND) {
        if self.slideshow_active {
            let _ = unsafe { KillTimer(Some(window), SLIDESHOW_TIMER) };
            self.slideshow_active = false;
            self.show_zoom_pill(window, "Slideshow: Stop".to_string());
            self.render(window);
        }
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

    /// 오버레이 내용 스냅샷 조립 (SPEC §3.6)
    fn overlay_content(&self, background: D2D1_COLOR_F) -> OverlayContent {
        let error_text = self
            .image_core
            .load_error
            .as_ref()
            .map(|(path, error)| overlay::build_error_text(path, &error.message, error.code));
        let info_text = if self.show_file_info {
            self.image_core.current.as_ref().map(|current| {
                let metadata = std::fs::metadata(&current.path).ok();
                overlay::build_info_text(
                    &current.path,
                    &current.image,
                    metadata.as_ref().map_or(0, std::fs::Metadata::len),
                    metadata.and_then(|metadata| metadata.modified().ok()),
                )
            })
        } else {
            None
        };
        // perceived brightness > 0.5 → 검정 에러 텍스트 (SPEC §3.6)
        let brightness = 0.299 * background.r + 0.587 * background.g + 0.114 * background.b;
        OverlayContent {
            error_text,
            info_text,
            zoom_pill_text: self.zoom_pill_text.clone(),
            background_is_bright: brightness > 0.5,
        }
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
        let content = self.overlay_content(background);
        let overlay = &self.overlay;
        let draw = |context: &_| overlay.draw(context, viewport.width, viewport.height, &content);
        if self
            .renderer
            .render(matrix, interpolation, background, draw)
            .is_err()
        {
            // 디바이스 로스트 — 재구축 후 1회 재시도
            if self.rebuild_renderer(window).is_ok() {
                let overlay = &self.overlay;
                let _ = self
                    .renderer
                    .render(matrix, interpolation, background, |context| {
                        overlay.draw(context, viewport.width, viewport.height, &content)
                    });
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
        let previous_scale = self.view_transform.scale;
        self.view_transform.zoom(factor, anchor, viewport, image);
        if (self.view_transform.scale - previous_scale).abs() > f32::EPSILON {
            let percent = (self.view_transform.scale / self.view_transform.device_pixel_ratio
                * 100.0)
                .round();
            self.show_zoom_pill(window, format!("Zoom: {percent}%"));
        }
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

/// 반환 = 이동 발생 여부 (슬라이드쇼 폴더 끝 취소 판단용)
fn execute_navigation(
    application: &mut Application,
    window: HWND,
    command: NavigationCommand,
) -> bool {
    match application.image_core.navigate(command) {
        // 캐시 히트 — 동기 표시 변경. 비동기 완료는 WM_APP_DECODE_COMPLETE에서 반영
        Some(true) => application.apply_current_image(window),
        Some(false) => {
            if application.image_core.load_error.is_some() {
                // 동기 실패(파일 접근 불가 등) — 에러 텍스트 표시
                application.apply_load_error(window);
            }
        }
        None => return false,
    }
    true
}

/// 외부 경로 열기(최근 파일·드롭·붙여넣기 공용) — 수동 로드 = 슬라이드쇼 취소 (SPEC §6.3)
fn open_external_path(application: &mut Application, window: HWND, path: &Path) {
    application.cancel_slideshow(window);
    if application.image_core.load_path(path) {
        application.apply_current_image(window);
    } else if application.image_core.load_error.is_some() {
        application.apply_load_error(window);
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
        Action::FirstFile => {
            execute_navigation(application, window, NavigationCommand::First);
        }
        Action::PreviousFile => {
            execute_navigation(application, window, NavigationCommand::Previous);
        }
        Action::NextFile => {
            execute_navigation(application, window, NavigationCommand::Next);
        }
        Action::LastFile => {
            execute_navigation(application, window, NavigationCommand::Last);
        }
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
            let pill = if application.view_transform.fit_tracking {
                "Fit"
            } else {
                "1:1"
            };
            application.show_zoom_pill(window, pill.to_string());
            application.render(window);
        }
        Action::PreserveZoom => {
            application.preserve_zoom = !application.preserve_zoom;
            let state = if application.preserve_zoom {
                "On"
            } else {
                "Off"
            };
            application.show_zoom_pill(window, format!("Preserve Zoom: {state}"));
            application.render(window);
        }
        Action::ShowFileInfo => {
            application.show_file_info = !application.show_file_info;
            application.render(window);
        }
        Action::Slideshow => application.toggle_slideshow(window),
        Action::Recent(index) => {
            let path = application
                .settings
                .recent_files()
                .get(usize::from(index))
                .map(|(_, path)| std::path::PathBuf::from(path));
            if let Some(path) = path {
                open_external_path(application, window, &path);
            }
        }
        Action::ClearRecents => {
            if application.settings.clear_recent_files() {
                unsafe { SetTimer(Some(window), RECENTS_SAVE_TIMER, 500, None) };
            }
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
        Action::Open => {
            let last_directory = application.settings.last_file_dialog_directory();
            let paths = open_dialog::show(window, last_directory.as_deref());
            // 다중 선택의 나머지 파일 = 새 창 (R7 멀티윈도우) — 현재는 첫 파일만
            if let Some(first) = paths.first() {
                if let Some(parent) = first.parent() {
                    application
                        .settings
                        .set_last_file_dialog_directory(&parent.to_string_lossy());
                    unsafe { SetTimer(Some(window), RECENTS_SAVE_TIMER, 500, None) };
                }
                let first = first.clone();
                open_external_path(application, window, &first);
            }
        }
        Action::OpenContainingFolder => {
            if let Some(current) = &application.image_core.current {
                file_ops::show_in_explorer(&current.path);
            }
        }
        Action::Delete | Action::DeletePermanent => {
            delete_current_file(application, window, action == Action::DeletePermanent);
        }
        Action::Rename => {
            rename_current_file(application, window);
        }
        Action::Copy => {
            if let Some(current) = &application.image_core.current {
                let image = current.image.clone();
                let path = current.path.clone();
                let orientation = BakedOrientation {
                    rotation_quadrant: application.view_transform.rotation_quadrant,
                    mirrored: application.view_transform.mirrored,
                    flipped: application.view_transform.flipped,
                };
                let _ = clipboard::copy_image(
                    window,
                    &path,
                    &image.frames[0].pixels,
                    image.pixel_width,
                    image.pixel_height,
                    &orientation,
                );
            }
        }
        Action::Paste => {
            // CF_HDROP 경로만 — 첫 항목 현재 창, 나머지 새 창은 R7 (SPEC §6.4)
            if let Some(first) = clipboard::paste_paths(window).first() {
                let first = first.clone();
                open_external_path(application, window, &first);
            }
        }
        Action::OpenWithOther => {
            // "다른 앱 선택" — OS Open With 다이얼로그 (SPEC §6.4)
            if let Some(current) = &application.image_core.current {
                let path = current.path.clone();
                open_with::show_open_with_dialog(window, &path);
            }
        }
        // OpenWith는 서브메뉴 컨테이너 — 항목 선택은 MenuSelection::OpenWithEntry 경로
        Action::OpenWith => {}
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

/// 삭제 흐름 (SPEC §6.4) — 확인 다이얼로그·afterdelete 이동·실패 시 재오픈
fn delete_current_file(application: &mut Application, window: HWND, permanent: bool) {
    let Some(path) = application
        .image_core
        .current
        .as_ref()
        .map(|current| current.path.clone())
    else {
        return;
    };
    // 영구 삭제는 항상 확인, 휴지통은 askdelete일 때만 (SPEC §6.4)
    if permanent || application.settings.options.ask_delete {
        let confirmation = file_ops::confirm_delete(window, &path, permanent);
        if !confirmation.confirmed {
            return;
        }
        if !permanent && confirmation.do_not_ask_again {
            application.settings.set_option_boolean("askdelete", false);
            unsafe { SetTimer(Some(window), RECENTS_SAVE_TIMER, 500, None) };
        }
    }
    // afterdelete 대상은 삭제 전에 계산: 0=이전 / 1=유지 / 2=다음 (SPEC §6.4)
    let target = match application.settings.options.after_delete {
        0 => application
            .image_core
            .peek_navigation_target(NavigationCommand::Previous),
        2 => application
            .image_core
            .peek_navigation_target(NavigationCommand::Next),
        _ => None,
    }
    .filter(|candidate| {
        !candidate
            .to_string_lossy()
            .eq_ignore_ascii_case(&path.to_string_lossy())
    });
    match file_ops::delete_file(&path, permanent) {
        Ok(()) => {
            application.image_core.refresh_folder();
            if let Some(target) = target {
                open_external_path(application, window, &target);
            }
        }
        Err(_) => {
            // 실패 시 파일 다시 열고 에러 표시 (SPEC §6.4)
            if application.image_core.reload_current() {
                application.apply_current_image(window);
            }
        }
    }
}

/// 이름 변경 흐름 (SPEC §6.4) — 다이얼로그·성공 시 새 경로 재오픈.
/// 디코더는 디코드 후 파일 핸들을 잡지 않으므로 별도 핸들 닫기 불필요.
fn rename_current_file(application: &mut Application, window: HWND) {
    let Some(path) = application
        .image_core
        .current
        .as_ref()
        .map(|current| current.path.clone())
    else {
        return;
    };
    let current_name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let Some(new_name) = dialogs::rename::show(window, &current_name) else {
        return;
    };
    if let Ok(new_path) = file_ops::rename_file(&path, &new_name) {
        application.image_core.refresh_folder();
        open_external_path(application, window, &new_path);
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
    // UI 스레드 = STA(OLE 포함 — 드래그&드롭), 디코드 워커 = MTA (PORTING_PLAN §3 매핑)
    unsafe { OleInitialize(None) }?;
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

    // 창 기본 크기 = 640×480 (SPEC §6.1, 2026-07-10 — 화면 비율 기반(40%×30%)은
    // 초광폭에서 부적합해 폐기. 지오메트리 복원은 R7)
    let window = unsafe {
        CreateWindowExW(
            Default::default(),
            class_name,
            w!("riv"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            640,
            480,
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
        application.drop_target = drag_drop::register(window).ok();
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
                if application.image_core.load_error.is_some() {
                    application.apply_load_error(window);
                } else {
                    application.apply_current_image(window);
                }
                application.ensure_window_shown(window);
            }
            LRESULT(0)
        }
        // 드롭 경로 수신 — 첫 파일 현재 창 (SPEC §5.4)
        WM_APP_DROP_PATH => {
            let path = unsafe { Box::from_raw(lparam.0 as *mut std::path::PathBuf) };
            if let Some(application) = unsafe { application_from_window(window) } {
                open_external_path(application, window, &path);
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
        // 줌 필 1초 자동 숨김 (SPEC §3.6)
        WM_TIMER if wparam.0 == ZOOM_PILL_TIMER => {
            let _ = unsafe { KillTimer(Some(window), ZOOM_PILL_TIMER) };
            if let Some(application) = unsafe { application_from_window(window) }
                && application.zoom_pill_text.take().is_some()
            {
                application.render(window);
            }
            LRESULT(0)
        }
        // 슬라이드쇼 틱 (SPEC §6.3) — 폴더 끝(루프 off) 도달 시 자동 취소
        WM_TIMER if wparam.0 == SLIDESHOW_TIMER => {
            if let Some(application) = unsafe { application_from_window(window) } {
                let command = if application.settings.options.slideshow_reversed {
                    NavigationCommand::Previous
                } else {
                    NavigationCommand::Next
                };
                if !execute_navigation(application, window, command) {
                    application.cancel_slideshow(window);
                }
            }
            LRESULT(0)
        }
        // 최근 파일 디바운스 저장 (SPEC §6.4)
        WM_TIMER if wparam.0 == RECENTS_SAVE_TIMER => {
            let _ = unsafe { KillTimer(Some(window), RECENTS_SAVE_TIMER) };
            if let Some(application) = unsafe { application_from_window(window) } {
                let _ = application.settings.save();
            }
            LRESULT(0)
        }
        // Open With 백그라운드 열거 시작 (250ms 디바운스 후 — SPEC §6.4)
        WM_TIMER if wparam.0 == OPEN_WITH_TIMER => {
            let _ = unsafe { KillTimer(Some(window), OPEN_WITH_TIMER) };
            if let Some(application) = unsafe { application_from_window(window) }
                && let Some(current) = &application.image_core.current
            {
                open_with::enumerate_in_background(window, current.path.clone());
            }
            LRESULT(0)
        }
        // Open With 열거 결과 수신 — 파일이 바뀌었으면 폐기 (SPEC §6.4)
        WM_APP_OPEN_WITH_LIST => {
            let list = unsafe { Box::from_raw(lparam.0 as *mut OpenWithList) };
            if let Some(application) = unsafe { application_from_window(window) } {
                let is_current = application
                    .image_core
                    .current
                    .as_ref()
                    .is_some_and(|current| {
                        current
                            .path
                            .to_string_lossy()
                            .eq_ignore_ascii_case(&list.path.to_string_lossy())
                    });
                if is_current {
                    application.open_with_list = Some(list);
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
                // 메뉴 구성 전 최근 파일 부재 감사 (SPEC §6.4)
                if application.settings.prune_recent_files() {
                    unsafe { SetTimer(Some(window), RECENTS_SAVE_TIMER, 500, None) };
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
                    slideshow_active: application.slideshow_active,
                    recent_names: application
                        .settings
                        .recent_files()
                        .into_iter()
                        .map(|(name, _)| name)
                        .collect(),
                    open_with_items: application.open_with_list.as_ref().map_or_else(
                        Vec::new,
                        |list| {
                            list.items
                                .iter()
                                .map(|item| (item.display_name.clone(), item.icon))
                                .collect()
                        },
                    ),
                    open_with_has_default: application
                        .open_with_list
                        .as_ref()
                        .is_some_and(|list| list.has_default),
                };
                match context_menu::show(window, state, x, y) {
                    Some(MenuSelection::Action(action)) => {
                        dispatch_action(application, window, action);
                    }
                    Some(MenuSelection::OpenWithEntry(index)) => {
                        // 셸 핸들러 Invoke — UI 스레드에서 재매칭 (SPEC §6.4)
                        if let (Some(current), Some(list)) = (
                            application.image_core.current.as_ref(),
                            application.open_with_list.as_ref(),
                        ) && let Some(item) = list.items.get(index)
                        {
                            let _ = open_with::invoke(&current.path, &item.executable_path);
                        }
                    }
                    None => {}
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
            let _ = unsafe { RevokeDragDrop(window) };
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
