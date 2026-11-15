use super::*;

impl Capabilities {
    /// The kernel explicitly implements the retired-context GPU clock query.
    pub const fn supports_gpu_timestamps(&self) -> bool {
        self.timestamps
    }
}

impl Context {
    /// Retire preceding dispatches, then sample the device GPU clock in ns.
    /// No host clock is used and failures from preceding work remain errors.
    pub fn timestamp_ns(&self) -> HandleResult<u64> {
        if !self.inner.device.capabilities.supports_gpu_timestamps() {
            return Err(HandleError::Unsupported);
        }
        self.inner.dispatcher.wait_idle()?;
        self.inner.raw.timestamp_ns()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_kernels_do_not_gain_timestamps_from_native_dialect_alone() {
        assert!(
            !Capabilities::from_negotiated_support(0, ISOLATED_NATIVE_DIALECT_TOKEN)
                .supports_gpu_timestamps()
        );
        assert!(
            Capabilities::from_negotiated_support(
                gpu_raw::GPU_EXECUTION_SUPPORT_TIMESTAMPS,
                ISOLATED_NATIVE_DIALECT_TOKEN
            )
            .supports_gpu_timestamps()
        );
    }
}
