#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AaStatus {
    pub(super) requested_msaa: u32,
    pub(super) effective_msaa: u32,
    pub(super) smaa_enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct AaRequests {
    pub(super) msaa: Option<u32>,
    pub(super) smaa: Option<bool>,
}

impl AaRequests {
    pub(super) fn is_empty(self) -> bool {
        self.msaa.is_none() && self.smaa.is_none()
    }

    pub(super) fn accepted_msaa(self, renderer_requested_msaa: u32) -> Option<u32> {
        self.msaa
            .filter(|requested| renderer_requested_msaa == *requested)
    }

    pub(super) fn accepted_smaa(self, renderer_smaa_enabled: bool) -> Option<bool> {
        self.smaa
            .filter(|enabled| renderer_smaa_enabled == *enabled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AaRuntime {
    requested_msaa: u32,
    effective_msaa: u32,
    requested_smaa: bool,
    smaa_enabled: bool,
    pending_msaa_request: Option<u32>,
    pending_smaa_request: Option<bool>,
}

impl AaRuntime {
    pub(super) fn new(requested_msaa: u32, requested_smaa: bool) -> Self {
        Self {
            requested_msaa,
            effective_msaa: 1,
            requested_smaa,
            smaa_enabled: requested_smaa,
            pending_msaa_request: None,
            pending_smaa_request: None,
        }
    }

    pub(super) fn status(&self) -> AaStatus {
        AaStatus {
            requested_msaa: self.requested_msaa,
            effective_msaa: self.effective_msaa,
            smaa_enabled: self.smaa_enabled,
        }
    }

    pub(super) fn initialize_msaa_from_renderer(
        &mut self,
        requested_msaa: u32,
        effective_msaa: u32,
    ) {
        self.requested_msaa = requested_msaa;
        self.effective_msaa = effective_msaa;
    }

    pub(super) fn initialize_smaa_from_renderer(
        &mut self,
        requested_smaa: bool,
        smaa_enabled: bool,
    ) {
        self.requested_smaa = requested_smaa;
        self.smaa_enabled = smaa_enabled;
    }

    pub(super) fn request_next_msaa(&mut self) {
        self.requested_msaa = next_msaa_sample_count(self.requested_msaa);
        self.pending_msaa_request = Some(self.requested_msaa);
    }

    pub(super) fn request_smaa_toggle(&mut self) {
        self.requested_smaa = !self.requested_smaa;
        self.pending_smaa_request = Some(self.requested_smaa);
    }

    pub(super) fn take_pending_requests(&mut self) -> AaRequests {
        AaRequests {
            msaa: self.pending_msaa_request.take(),
            smaa: self.pending_smaa_request.take(),
        }
    }

    pub(super) fn sync_after_application(
        &mut self,
        renderer_requested_msaa: u32,
        renderer_effective_msaa: u32,
        renderer_smaa_enabled: bool,
    ) {
        self.requested_msaa = renderer_requested_msaa;
        self.effective_msaa = renderer_effective_msaa;
        self.requested_smaa = renderer_smaa_enabled;
        self.smaa_enabled = renderer_smaa_enabled;
    }
}

fn next_msaa_sample_count(requested: u32) -> u32 {
    match requested {
        1 => 2,
        2 => 4,
        4 => 8,
        8 => 1,
        _ => 2,
    }
}

#[cfg(test)]
impl AaRuntime {
    pub(in crate::widget) fn requested_smaa(&self) -> bool {
        self.requested_smaa
    }

    pub(in crate::widget) fn pending_requests(&self) -> AaRequests {
        AaRequests {
            msaa: self.pending_msaa_request,
            smaa: self.pending_smaa_request,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msaa_cycle_wraps_and_sanitizes() {
        assert_eq!(next_msaa_sample_count(1), 2);
        assert_eq!(next_msaa_sample_count(2), 4);
        assert_eq!(next_msaa_sample_count(4), 8);
        assert_eq!(next_msaa_sample_count(8), 1);
        assert_eq!(next_msaa_sample_count(16), 2);
    }
}
