//! 변환 모델 (SPEC §3.2 — DP1·DP2·DP7). 렌더 API와 무관한 순수 수식.
//! fit·줌·팬 지오메트리는 mpv `video/out/aspect.c` 로직 참고.

/// fit 기준 축 (SPEC §8.2 `fitmode`: 0=Width/1=Height)
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    Width,
    Height,
}

impl FitMode {
    /// 설정값 `fitmode` → 축, 범위 밖은 기본(Width)
    pub fn from_setting(value: u32) -> Self {
        if value == 1 {
            Self::Height
        } else {
            Self::Width
        }
    }
}

/// 논리 줌 한계 — 10% ~ 500%, 초과 요청은 무시 (SPEC §3.2)
const MINIMUM_LOGICAL_ZOOM: f32 = 0.1;
const MAXIMUM_LOGICAL_ZOOM: f32 = 5.0;

/// 뷰포트·이미지 크기 (디바이스 픽셀 / 이미지 픽셀)
#[derive(Clone, Copy)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

pub struct ViewTransform {
    /// DP1 — 이미지 픽셀당 디바이스 픽셀 절대 배율. 1.0 = 픽셀 퍼펙트
    pub scale: f32,
    /// 현재 배율이 fit에서 파생됐는지 — fit 상태면 render 시점에 재계산
    pub fit_tracking: bool,
    /// 90° 사분면 회전 (0..=3)
    pub rotation_quadrant: u32,
    pub mirrored: bool,
    pub flipped: bool,
    /// 이미지 중심 − 뷰포트 중심 (디바이스 픽셀)
    pub pan_offset_x: f32,
    pub pan_offset_y: f32,
    pub fit_mode: FitMode,
    /// 논리 1:1(줌 100%)에 해당하는 배율 (창 DPI / 96)
    pub device_pixel_ratio: f32,
}

impl ViewTransform {
    pub fn new(device_pixel_ratio: f32) -> Self {
        Self {
            scale: 1.0,
            fit_tracking: true,
            rotation_quadrant: 0,
            mirrored: false,
            flipped: false,
            pan_offset_x: 0.0,
            pan_offset_y: 0.0,
            fit_mode: FitMode::Width,
            device_pixel_ratio,
        }
    }

    /// 회전 반영 후 화면 축 기준 이미지 크기 (이미지 픽셀)
    pub fn rotated_image_size(&self, image: Size) -> Size {
        if !self.rotation_quadrant.is_multiple_of(2) {
            Size {
                width: image.height,
                height: image.width,
            }
        } else {
            image
        }
    }

    /// fitScale — 렌더타깃(디바이스 픽셀) 기준, fit 축 비율 (SPEC §3.2)
    pub fn fit_scale(&self, viewport: Size, image: Size) -> f32 {
        let rotated = self.rotated_image_size(image);
        match self.fit_mode {
            FitMode::Width => viewport.width / rotated.width,
            FitMode::Height => viewport.height / rotated.height,
        }
    }

    /// fit 상태로 재정렬 (배율 재계산 + 팬 리셋)
    pub fn refit(&mut self, viewport: Size, image: Size) {
        self.scale = self.fit_scale(viewport, image);
        self.fit_tracking = true;
        self.pan_offset_x = 0.0;
        self.pan_offset_y = 0.0;
    }

    /// render 시점 동기화 — fit 추적 중이면 현재 뷰포트로 배율 재계산 (플래시 방지 규칙)
    pub fn synchronize(&mut self, viewport: Size, image: Size) {
        if self.fit_tracking {
            self.scale = self.fit_scale(viewport, image);
        }
        self.clamp_pan(viewport, image);
    }

    /// 곱셈 줌. `cursor_from_center`가 Some이고 확대 방향 & 새 배율 > fit이면 커서 앵커,
    /// 아니면 중앙 재정렬 (SPEC §3.2)
    pub fn zoom(
        &mut self,
        factor: f32,
        cursor_from_center: Option<(f32, f32)>,
        viewport: Size,
        image: Size,
    ) {
        let new_scale = self.scale * factor;
        let logical = new_scale / self.device_pixel_ratio;
        if !(MINIMUM_LOGICAL_ZOOM..=MAXIMUM_LOGICAL_ZOOM).contains(&logical) {
            return; // 한계 초과 요청은 무시
        }
        let cursor_anchor = cursor_from_center
            .filter(|_| factor > 1.0 && new_scale > self.fit_scale(viewport, image));
        match cursor_anchor {
            Some((cursor_x, cursor_y)) => {
                // 커서 아래 텍셀 고정: pan −= (cursor − pan) × (factor − 1)
                self.pan_offset_x -= (cursor_x - self.pan_offset_x) * (factor - 1.0);
                self.pan_offset_y -= (cursor_y - self.pan_offset_y) * (factor - 1.0);
            }
            None => {
                // 중앙 재정렬 — 뷰포트 중앙 텍셀 고정
                self.pan_offset_x *= factor;
                self.pan_offset_y *= factor;
            }
        }
        self.scale = new_scale;
        self.fit_tracking = false;
        self.clamp_pan(viewport, image);
    }

    /// Toggle Zoom — fit ↔ 1:1(=device_pixel_ratio) (SPEC §3.2).
    /// fit → 1:1이 확대 방향이면 `cursor_from_center` 앵커, 아니면 중앙 정렬.
    /// 곱셈 줌이 아닌 직접 대입 — 1:1 정확도(DP7 픽셀 스냅)·논리 줌 한계 미적용.
    pub fn toggle_zoom(
        &mut self,
        cursor_from_center: Option<(f32, f32)>,
        viewport: Size,
        image: Size,
    ) {
        if self.fit_tracking {
            let factor = self.device_pixel_ratio / self.fit_scale(viewport, image);
            self.scale = self.device_pixel_ratio;
            self.fit_tracking = false;
            // 커서 아래 텍셀 고정 — fit의 팬은 (0,0)이므로 −cursor × (factor − 1)
            let (pan_x, pan_y) = match cursor_from_center.filter(|_| factor > 1.0) {
                Some((cursor_x, cursor_y)) => {
                    (-cursor_x * (factor - 1.0), -cursor_y * (factor - 1.0))
                }
                None => (0.0, 0.0),
            };
            self.pan_offset_x = pan_x;
            self.pan_offset_y = pan_y;
            self.clamp_pan(viewport, image);
        } else {
            self.refit(viewport, image);
        }
    }

    /// 사분면 회전. 회전 전 1:1이면 유지, 아니면 refit (SPEC §3.2 회전 시 줌 정책)
    pub fn rotate(&mut self, quadrant_step: i32, viewport: Size, image: Size) {
        self.rotation_quadrant =
            (self.rotation_quadrant as i32 + quadrant_step).rem_euclid(4) as u32;
        if (self.scale - self.device_pixel_ratio).abs() < f32::EPSILON && !self.fit_tracking {
            self.pan_offset_x = 0.0;
            self.pan_offset_y = 0.0;
            self.clamp_pan(viewport, image);
        } else {
            self.refit(viewport, image);
        }
    }

    /// mirror/flip — 바운딩 박스 불변이므로 줌·팬 불변 (SPEC §3.2)
    pub fn mirror(&mut self) {
        self.mirrored = !self.mirrored;
    }

    pub fn flip(&mut self) {
        self.flipped = !self.flipped;
    }

    pub fn pan_by(&mut self, delta_x: f32, delta_y: f32, viewport: Size, image: Size) {
        self.pan_offset_x += delta_x;
        self.pan_offset_y += delta_y;
        self.clamp_pan(viewport, image);
    }

    /// DP2 — 팬 클램프: 이미지가 뷰포트보다 크면 갭 없는 범위, 작으면 중앙 고정
    pub fn clamp_pan(&mut self, viewport: Size, image: Size) {
        let rotated = self.rotated_image_size(image);
        let maximum_x = ((rotated.width * self.scale - viewport.width) / 2.0).max(0.0);
        let maximum_y = ((rotated.height * self.scale - viewport.height) / 2.0).max(0.0);
        self.pan_offset_x = self.pan_offset_x.clamp(-maximum_x, maximum_x);
        self.pan_offset_y = self.pan_offset_y.clamp(-maximum_y, maximum_y);
    }

    /// 아핀 행렬 [M11, M12, M21, M22, M31, M32] — 이미지 픽셀 → 디바이스 픽셀.
    /// 구성: 이미지 중심 원점 이동 → mirror/flip·배율 → 90°q 회전 → 뷰포트 중심+팬 이동.
    /// DP7 — 축정렬(0/180) & 배율 1.0 또는 dpr일 때 정수 픽셀 스냅.
    pub fn matrix(&self, viewport: Size, image: Size) -> [f32; 6] {
        let (cosine, sine) = match self.rotation_quadrant {
            0 => (1.0, 0.0),
            1 => (0.0, 1.0),
            2 => (-1.0, 0.0),
            _ => (0.0, -1.0),
        };
        let scale_x = self.scale * if self.mirrored { -1.0 } else { 1.0 };
        let scale_y = self.scale * if self.flipped { -1.0 } else { 1.0 };
        let center_x = image.width / 2.0;
        let center_y = image.height / 2.0;
        let mut translate_x = viewport.width / 2.0 + self.pan_offset_x;
        let mut translate_y = viewport.height / 2.0 + self.pan_offset_y;

        let axis_aligned = self.rotation_quadrant.is_multiple_of(2);
        let snappable_scale = (self.scale - 1.0).abs() < f32::EPSILON
            || (self.scale - self.device_pixel_ratio).abs() < f32::EPSILON;
        if axis_aligned && snappable_scale {
            // 텍셀 그리드가 정수 디바이스 픽셀에 오도록 최종 이동만 반올림
            let origin_x = translate_x - center_x * scale_x * cosine + center_y * scale_y * sine;
            let origin_y = translate_y - center_x * scale_x * sine - center_y * scale_y * cosine;
            translate_x += origin_x.round() - origin_x;
            translate_y += origin_y.round() - origin_y;
        }

        [
            scale_x * cosine,
            scale_x * sine,
            -scale_y * sine,
            scale_y * cosine,
            -center_x * scale_x * cosine + center_y * scale_y * sine + translate_x,
            -center_x * scale_x * sine - center_y * scale_y * cosine + translate_y,
        ]
    }
}
