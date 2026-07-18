//! Pure view-transform math: fit, zoom, pan, rotation.

/// Fit axis, from the `fitmode` setting.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    Width,
    Height,
}

impl FitMode {
    pub fn from_setting(value: u32) -> Self {
        if value == 1 {
            Self::Height
        } else {
            Self::Width
        }
    }
}

/// Logical zoom limits; incremental zoom clamps to these.
const MINIMUM_LOGICAL_ZOOM: f32 = 0.1;
const MAXIMUM_LOGICAL_ZOOM: f32 = 5.0;

#[derive(Clone, Copy)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

pub struct ViewTransform {
    pub scale: f32,
    /// While true the scale is recomputed from the viewport at render time.
    pub fit_tracking: bool,
    pub rotation_quadrant: u32,
    pub mirrored: bool,
    pub flipped: bool,
    pub pan_offset_x: f32,
    pub pan_offset_y: f32,
    pub fit_mode: FitMode,
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

    pub fn fit_scale(&self, viewport: Size, image: Size) -> f32 {
        let rotated = self.rotated_image_size(image);
        match self.fit_mode {
            FitMode::Width => viewport.width / rotated.width,
            FitMode::Height => viewport.height / rotated.height,
        }
    }

    pub fn refit(&mut self, viewport: Size, image: Size) {
        self.scale = self.fit_scale(viewport, image);
        self.fit_tracking = true;
        self.pan_offset_x = 0.0;
        self.pan_offset_y = 0.0;
    }

    pub fn synchronize(&mut self, viewport: Size, image: Size) {
        if self.fit_tracking {
            self.scale = self.fit_scale(viewport, image);
        }
        self.clamp_pan(viewport, image);
    }

    pub fn zoom(
        &mut self,
        factor: f32,
        cursor_from_center: Option<(f32, f32)>,
        viewport: Size,
        image: Size,
    ) {
        let current_logical = self.scale / self.device_pixel_ratio;
        // Limits stretch to the current scale so an out-of-range fit can step back in.
        let logical = (current_logical * factor).clamp(
            MINIMUM_LOGICAL_ZOOM.min(current_logical),
            MAXIMUM_LOGICAL_ZOOM.max(current_logical),
        );
        if logical == current_logical {
            return; // no movement toward the limits
        }
        let new_scale = logical * self.device_pixel_ratio;
        let factor = new_scale / self.scale;
        // Cursor anchor only when enlarging beyond fit.
        let cursor_anchor = cursor_from_center
            .filter(|_| factor > 1.0 && new_scale > self.fit_scale(viewport, image));
        match cursor_anchor {
            Some((cursor_x, cursor_y)) => {
                self.pan_offset_x -= (cursor_x - self.pan_offset_x) * (factor - 1.0);
                self.pan_offset_y -= (cursor_y - self.pan_offset_y) * (factor - 1.0);
            }
            None => {
                self.pan_offset_x *= factor;
                self.pan_offset_y *= factor;
            }
        }
        self.scale = new_scale;
        self.fit_tracking = false;
        self.clamp_pan(viewport, image);
    }

    /// Fit <-> 1:1; assigns the scale directly so pixel snapping stays exact.
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

    /// Pan bounds: no gaps when larger than the viewport, centered when smaller.
    pub fn clamp_pan(&mut self, viewport: Size, image: Size) {
        let rotated = self.rotated_image_size(image);
        let maximum_x = ((rotated.width * self.scale - viewport.width) / 2.0).max(0.0);
        let maximum_y = ((rotated.height * self.scale - viewport.height) / 2.0).max(0.0);
        self.pan_offset_x = self.pan_offset_x.clamp(-maximum_x, maximum_x);
        self.pan_offset_y = self.pan_offset_y.clamp(-maximum_y, maximum_y);
    }

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
        // Snap the final translation so the texel grid lands on device pixels.
        if axis_aligned && snappable_scale {
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

#[cfg(test)]
mod zoom_limit_tests {
    use super::*;

    const VIEWPORT: Size = Size {
        width: 640.0,
        height: 480.0,
    };
    const HUGE_IMAGE: Size = Size {
        width: 17000.0,
        height: 12750.0,
    };
    const TINY_IMAGE: Size = Size {
        width: 16.0,
        height: 12.0,
    };
    const PLAIN_IMAGE: Size = Size {
        width: 800.0,
        height: 600.0,
    };

    fn transform_at(scale: f32, device_pixel_ratio: f32) -> ViewTransform {
        let mut transform = ViewTransform::new(device_pixel_ratio);
        transform.scale = scale;
        transform.fit_tracking = false;
        transform
    }

    #[test]
    fn zoom_in_climbs_out_of_a_fit_below_the_minimum() {
        let mut transform = ViewTransform::new(1.0);
        transform.refit(VIEWPORT, HUGE_IMAGE);
        assert!(transform.scale < MINIMUM_LOGICAL_ZOOM);
        let mut steps = 0;
        while transform.scale < MINIMUM_LOGICAL_ZOOM && steps < 32 {
            let before = transform.scale;
            transform.zoom(1.25, None, VIEWPORT, HUGE_IMAGE);
            assert!(transform.scale > before);
            steps += 1;
        }
        assert!(transform.scale >= MINIMUM_LOGICAL_ZOOM);
    }

    #[test]
    fn zoom_out_from_a_fit_below_the_minimum_is_ignored() {
        let mut transform = ViewTransform::new(1.0);
        transform.refit(VIEWPORT, HUGE_IMAGE);
        let fit = transform.scale;
        transform.zoom(0.8, None, VIEWPORT, HUGE_IMAGE);
        assert_eq!(transform.scale, fit);
        assert!(transform.fit_tracking);
    }

    #[test]
    fn zoom_steps_land_exactly_on_the_boundaries() {
        let mut transform = transform_at(4.5, 1.0);
        transform.zoom(1.25, None, VIEWPORT, PLAIN_IMAGE);
        assert_eq!(transform.scale, MAXIMUM_LOGICAL_ZOOM);
        transform.zoom(1.25, None, VIEWPORT, PLAIN_IMAGE);
        assert_eq!(transform.scale, MAXIMUM_LOGICAL_ZOOM);

        let mut transform = transform_at(0.11, 1.0);
        transform.zoom(0.8, None, VIEWPORT, PLAIN_IMAGE);
        assert_eq!(transform.scale, MINIMUM_LOGICAL_ZOOM);
        transform.zoom(0.8, None, VIEWPORT, PLAIN_IMAGE);
        assert_eq!(transform.scale, MINIMUM_LOGICAL_ZOOM);
    }

    #[test]
    fn zoom_out_moves_inward_from_a_fit_above_the_maximum() {
        let mut transform = ViewTransform::new(1.0);
        transform.refit(VIEWPORT, TINY_IMAGE);
        assert!(transform.scale > MAXIMUM_LOGICAL_ZOOM);
        let fit = transform.scale;
        transform.zoom(1.25, None, VIEWPORT, TINY_IMAGE);
        assert_eq!(transform.scale, fit);
        transform.zoom(0.8, None, VIEWPORT, TINY_IMAGE);
        assert!(transform.scale < fit && transform.scale > MAXIMUM_LOGICAL_ZOOM);
    }

    #[test]
    fn limits_apply_in_the_logical_domain() {
        let mut transform = transform_at(9.5, 2.0);
        transform.zoom(1.25, None, VIEWPORT, PLAIN_IMAGE);
        assert_eq!(transform.scale, MAXIMUM_LOGICAL_ZOOM * 2.0);
    }
}
