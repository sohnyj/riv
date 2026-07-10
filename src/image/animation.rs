//! 애니메이션 프레임 스케줄러 상태 (SPEC §4.6) — 프레임 지연 × (100/speed).
//!
//! 타이머 구동(SetTimer 재예약)은 창 쪽이 담당하고, 여기는 프레임 인덱스·지연·
//! 속도·일시정지 상태만 관리한다. 프레임 타이밍은 지연 재예약 방식
//! (mpv `player/video.c` 프레임 스케줄링 참고 — 드랍 없이 다음 프레임 예약).

use super::decode::DecodedImage;

/// 재생 속도 클램프 50~300%, ±25%p 스텝, 기본 100% (SPEC §4.6 — 2026-07-10 축소)
const SPEED_MINIMUM_PERCENT: u32 = 50;
const SPEED_MAXIMUM_PERCENT: u32 = 300;
const SPEED_STEP_PERCENT: u32 = 25;
const SPEED_DEFAULT_PERCENT: u32 = 100;

pub struct Animation {
    frame_delays_milliseconds: Vec<u32>,
    pub frame_index: usize,
    pub paused: bool,
    speed_percent: u32,
}

impl Animation {
    /// 프레임 수 ≠ 1일 때만 애니메이션 (SPEC §4.6 — 정지 이미지는 스케줄러 비대상)
    pub fn new(image: &DecodedImage) -> Option<Self> {
        (image.frames.len() > 1).then(|| Self {
            frame_delays_milliseconds: image
                .frames
                .iter()
                .map(|frame| frame.delay_milliseconds)
                .collect(),
            frame_index: 0,
            paused: false,
            speed_percent: SPEED_DEFAULT_PERCENT,
        })
    }

    /// 현재 프레임의 표시 지연 = 프레임 지연 × (100/speed) (SPEC §4.6)
    pub fn current_delay_milliseconds(&self) -> u32 {
        let delay = self.frame_delays_milliseconds[self.frame_index].max(1);
        (delay * 100 / self.speed_percent).max(1)
    }

    /// 다음 프레임으로 순환 진행 — 반환 = 새 인덱스 (타이머 틱·Next Frame 공용)
    pub fn advance(&mut self) -> usize {
        self.frame_index = (self.frame_index + 1) % self.frame_delays_milliseconds.len();
        self.frame_index
    }

    /// ±25%p 스텝, 50~300% 클램프 (SPEC §4.6)
    pub fn adjust_speed(&mut self, increase: bool) {
        self.speed_percent = if increase {
            (self.speed_percent + SPEED_STEP_PERCENT).min(SPEED_MAXIMUM_PERCENT)
        } else {
            (self.speed_percent - SPEED_STEP_PERCENT).max(SPEED_MINIMUM_PERCENT)
        };
    }

    pub fn reset_speed(&mut self) {
        self.speed_percent = SPEED_DEFAULT_PERCENT;
    }
}
