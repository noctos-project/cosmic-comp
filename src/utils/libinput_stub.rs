// SPDX-License-Identifier: GPL-3.0-only

//! Stand-in for the libinput device type on builds without the kms backend.
//!
//! Shared code keeps per-device state (keyboard LEDs, per-device config) keyed
//! on libinput devices and finds them by downcasting. Without libinput no such
//! device can exist, so this type is uninhabited: every downcast yields `None`
//! and the methods below are never called.

use smithay::input::keyboard::LedState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDevice {}

impl InputDevice {
    pub fn id(&self) -> String {
        match *self {}
    }
    pub fn name(&self) -> &str {
        match *self {}
    }
    pub fn config_tap_finger_count(&self) -> u32 {
        match *self {}
    }
    pub fn led_update(&mut self, _leds: LedState) {
        match *self {}
    }
}
