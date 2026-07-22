//! Animation frame scheduler state; the window drives the timer.

use super::decode::DecodedImage;

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

    /// A frame delay at the current speed, floored at 1ms.
    fn scaled_delay(&self, delay: u32) -> u32 {
        (delay.max(1) * 100 / self.speed_percent).max(1)
    }

    pub fn current_delay_milliseconds(&self) -> u32 {
        self.scaled_delay(self.frame_delays_milliseconds[self.frame_index])
    }

    /// Time for one pass through every frame at the current speed.
    pub fn loop_duration_milliseconds(&self) -> u32 {
        self.frame_delays_milliseconds
            .iter()
            .map(|&delay| self.scaled_delay(delay))
            .fold(0u32, u32::saturating_add)
    }

    pub fn next_frame(&mut self) -> usize {
        self.frame_index = (self.frame_index + 1) % self.frame_delays_milliseconds.len();
        self.frame_index
    }

    pub fn previous_frame(&mut self) -> usize {
        let count = self.frame_delays_milliseconds.len();
        self.frame_index = (self.frame_index + count - 1) % count;
        self.frame_index
    }

    pub fn frame_count(&self) -> usize {
        self.frame_delays_milliseconds.len()
    }

    pub fn adjust_speed(&mut self, increase: bool) {
        self.speed_percent = if increase {
            (self.speed_percent + SPEED_STEP_PERCENT).min(SPEED_MAXIMUM_PERCENT)
        } else {
            (self.speed_percent - SPEED_STEP_PERCENT).max(SPEED_MINIMUM_PERCENT)
        };
    }

    pub fn speed_percent(&self) -> u32 {
        self.speed_percent
    }

    pub fn reset_speed(&mut self) {
        self.speed_percent = SPEED_DEFAULT_PERCENT;
    }
}
