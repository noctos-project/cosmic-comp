// SPDX-License-Identifier: GPL-3.0-only

use crate::state::{BackendData, State};
use smithay::wayland::drm_syncobj::{DrmSyncobjHandler, DrmSyncobjState};

impl DrmSyncobjHandler for State {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        match &mut self.backend {
            #[cfg(feature = "backend-kms")]
            BackendData::Kms(kms) => kms.syncobj_state.as_mut(),
            _ => None,
        }
    }
}
