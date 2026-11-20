// SPDX-License-Identifier: GPL-3.0-only

//! illumos console framebuffer backend (NoctOS).
//!
//! SKELETON: the state type and the methods shared code dispatches to exist so
//! the tree builds without the Linux session backends; `init_backend` does not
//! bring anything up yet. Plan: surfaceless EGL + GlowRenderer rendering
//! offscreen, read back and blitted to /dev/fb, with VUID keyboard/pointer
//! input (the `illumosfb` crate provides both device layers).

use crate::{config::ScreenFilter, state::State};
use anyhow::{Result, anyhow};
use smithay::{
    backend::renderer::{damage::OutputDamageTracker, glow::GlowRenderer},
    output::Output,
    reexports::{calloop::EventLoop, wayland_server::DisplayHandle},
};

use super::render::ScreenFilterStorage;

#[derive(Debug)]
pub struct IllumosfbState {
    pub renderer: GlowRenderer,
    output: Output,
    #[allow(dead_code)]
    damage_tracker: OutputDamageTracker,
    screen_filter_state: ScreenFilterStorage,
}

impl IllumosfbState {
    pub fn schedule_render(&mut self, _output: &Output) {
        // TODO(noctos): ping the render source once the render loop exists.
    }

    pub fn all_outputs(&self) -> Vec<Output> {
        vec![self.output.clone()]
    }

    pub fn apply_config_for_outputs(&mut self, _test_only: bool) -> Result<()> {
        // One fixed mode: whatever the console framebuffer was booted in.
        Ok(())
    }

    pub fn update_screen_filter(&mut self, screen_filter: &ScreenFilter) -> Result<()> {
        self.screen_filter_state.filter = screen_filter.clone();
        Ok(())
    }
}

pub fn init_backend(
    _dh: &DisplayHandle,
    _event_loop: &mut EventLoop<State>,
    _state: &mut State,
) -> Result<()> {
    Err(anyhow!("illumosfb backend: bring-up is not implemented yet"))
}
