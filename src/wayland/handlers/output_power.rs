// SPDX-License-Identifier: GPL-3.0-only

use smithay::output::Output;

#[cfg(feature = "backend-kms")]
use crate::{backend::kms::Surface, state::BackendData};
use crate::{
    state::State,
    utils::prelude::OutputExt,
    wayland::protocols::output_power::{
        OutputPowerHandler, OutputPowerState, delegate_output_power,
    },
};

/// Without kms there is no DPMS control: outputs are always on.
#[cfg(not(feature = "backend-kms"))]
pub fn set_all_surfaces_dpms_on(_state: &mut State) {}

#[cfg(feature = "backend-kms")]
pub fn set_all_surfaces_dpms_on(state: &mut State) {
    let mut changed = false;
    for surface in kms_surfaces(state) {
        if !surface.get_dpms() {
            surface.set_dpms(true);
            changed = true;
        }
    }

    if changed {
        // Powering an output on because of input is activity. Without this the idle
        // notifier stays idled, the idle daemon never sees `resumed`, and nothing
        // powers the output off again until real input arrives.
        let seat = state.common.shell.read().seats.last_active().clone();
        state.common.idle_notifier_state.notify_activity(&seat);
        OutputPowerState::refresh(state);
    }
}

#[cfg(feature = "backend-kms")]
fn kms_surfaces(state: &mut State) -> impl Iterator<Item = &mut Surface> {
    if let BackendData::Kms(kms_state) = &mut state.backend {
        Some(
            kms_state
                .drm_devices
                .values_mut()
                .flat_map(|device| device.inner.surfaces.values_mut()),
        )
    } else {
        None
    }
    .into_iter()
    .flatten()
}

// Get KMS `Surface` for output, and for all outputs mirroring it
#[cfg(feature = "backend-kms")]
fn kms_surfaces_for_output<'a>(
    state: &'a mut State,
    output: &'a Output,
) -> impl Iterator<Item = &'a mut Surface> + 'a {
    kms_surfaces(state).filter(move |surface| {
        surface.output == *output || surface.output.mirroring().as_ref() == Some(output)
    })
}

// Get KMS `Surface` for output
#[cfg(feature = "backend-kms")]
fn primary_kms_surface_for_output<'a>(
    state: &'a mut State,
    output: &Output,
) -> Option<&'a mut Surface> {
    kms_surfaces(state).find(|surface| surface.output == *output)
}

impl OutputPowerHandler for State {
    fn output_power_state(&mut self) -> &mut OutputPowerState {
        &mut self.common.output_power_state
    }

    #[cfg(feature = "backend-kms")]
    fn get_dpms(&mut self, output: &Output) -> Option<bool> {
        let surface = primary_kms_surface_for_output(self, output)?;
        Some(surface.get_dpms())
    }

    #[cfg(feature = "backend-kms")]
    fn set_dpms(&mut self, output: &Output, on: bool) {
        for surface in kms_surfaces_for_output(self, output) {
            surface.set_dpms(on);
        }
    }

    #[cfg(not(feature = "backend-kms"))]
    fn get_dpms(&mut self, _output: &Output) -> Option<bool> {
        None
    }

    #[cfg(not(feature = "backend-kms"))]
    fn set_dpms(&mut self, _output: &Output, _on: bool) {}
}

delegate_output_power!(State);
