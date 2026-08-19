// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: MIT

//! The ESP32-S3-BOX-3 backend: an [`slint_safeui_app::Platform`] whose clock is
//! embassy's, whose input is the GT911 touch controller, and whose frame buffer
//! ends up on the ILI9342C panel.

use alloc::boxed::Box;
use core::time::Duration;

use embedded_graphics_core::pixelcolor::Rgb565;
use esp_hal::gpio::Output;
use slint_safeui_app::{AppEvent, Platform, slint_sc};

use crate::board::{BoardDisplay, BoardI2c, PANEL_HEIGHT, PANEL_WIDTH};

/// The scene positions its telltales in absolute pixels for a 640x480 window,
/// and the Slint SC subset has no division to make that relative. The panel is
/// exactly half that in each direction, so the backend renders the scene at its
/// design size and averages every 2x2 block down to one panel pixel. `downsample`
/// assumes that ratio, so `SCALE` isn't a free parameter.
const SCALE: u32 = 2;
const SCENE_WIDTH: u32 = PANEL_WIDTH * SCALE;
const SCENE_HEIGHT: u32 = PANEL_HEIGHT * SCALE;

/// How long the GT911 may go unread. It has an interrupt line, but the board
/// wires it to the pin that selects the I2C address at reset, so polling it is
/// what the vendor's driver does too.
const TOUCH_POLL_INTERVAL: embassy_time::Duration = embassy_time::Duration::from_millis(20);

pub struct Esp32Platform {
    display: BoardDisplay,
    touch: gt911::Gt911Blocking<BoardI2c>,
    i2c: BoardI2c,
    /// `width * height * 3` bytes of PSRAM, at the scene's size.
    frame: Box<[u8]>,
    /// Where the ongoing touch last was, so that the release can be placed.
    pressed_at: Option<slint_sc::Point>,
    // Kept alive for the lifetime of the backend.
    _backlight: Output<'static>,
    _touch_int: Output<'static>,
}

impl Esp32Platform {
    pub fn new(
        display: BoardDisplay,
        touch: gt911::Gt911Blocking<BoardI2c>,
        i2c: BoardI2c,
        backlight: Output<'static>,
        touch_int: Output<'static>,
    ) -> Self {
        let frame = alloc::vec![0u8; (SCENE_WIDTH * SCENE_HEIGHT * 3) as usize].into_boxed_slice();
        Self {
            display,
            touch,
            i2c,
            frame,
            pressed_at: None,
            _backlight: backlight,
            _touch_int: touch_int,
        }
    }
}

impl Platform for Esp32Platform {
    fn now(&self) -> Duration {
        Duration::from_micros(embassy_time::Instant::now().as_micros())
    }

    fn size(&self) -> slint_sc::Size {
        slint_sc::Size::new(SCENE_WIDTH, SCENE_HEIGHT)
    }

    fn get_input_event(&mut self) -> Option<AppEvent> {
        // One read of the controller per call, so the caller's drain loop ends.
        match self.touch.get_touch(&mut self.i2c) {
            Ok(Some(point)) => {
                let position = slint_sc::Point::new(
                    point.x as i32 * SCALE as i32,
                    point.y as i32 * SCALE as i32,
                );
                // A finger that stays down only updates where the release lands.
                self.pressed_at
                    .replace(position)
                    .is_none()
                    .then(|| AppEvent::Touch(slint_sc::TouchEvent::pressed(position)))
            }
            Ok(None) => self
                .pressed_at
                .take()
                .map(|position| AppEvent::Touch(slint_sc::TouchEvent::released(position))),
            Err(_) => None,
        }
    }

    async fn wait_for_more_events(&mut self, timeout: Option<Duration>) {
        let wait = timeout.map_or(TOUCH_POLL_INTERVAL, |timeout| {
            TOUCH_POLL_INTERVAL.min(embassy_time::Duration::from_micros(timeout.as_micros() as u64))
        });
        embassy_time::Timer::after(wait).await;
    }

    fn with_frame_buffer(&mut self, render: impl FnOnce(&mut [u8])) {
        let Self { display, frame, .. } = self;
        render(frame);

        let pixels = (0..PANEL_HEIGHT)
            .flat_map(|y| (0..PANEL_WIDTH).map(move |x| (x, y)))
            .map(|(x, y)| downsample(frame, x, y));
        display
            .set_pixels(0, 0, (PANEL_WIDTH - 1) as u16, (PANEL_HEIGHT - 1) as u16, pixels)
            .expect("the panel accepts a full-screen update");
    }
}

/// The panel pixel at (`x`, `y`): the mean of the 2x2 scene pixels behind it,
/// converted to the panel's RGB565.
fn downsample(frame: &[u8], x: u32, y: u32) -> Rgb565 {
    let stride = (SCENE_WIDTH * 3) as usize;
    let top_left = (y * SCALE) as usize * stride + (x * SCALE * 3) as usize;

    let mut sum = [0u16; 3];
    for row in [top_left, top_left + stride] {
        for pixel in [row, row + 3] {
            for (channel, value) in sum.iter_mut().zip(&frame[pixel..pixel + 3]) {
                *channel += *value as u16;
            }
        }
    }
    let [r, g, b] = sum.map(|channel| (channel / 4) as u8);
    Rgb565::new(r >> 3, g >> 2, b >> 3)
}
