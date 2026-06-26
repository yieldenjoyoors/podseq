//! Runtime bridge to Commonware: defines the combined `Runtime` bound and re-exports the tokio backend.

#![forbid(unsafe_code)]

pub use commonware_runtime::{
    self,
    tokio::{Config, Context, Runner},
    Clock, Metrics, Network, Runner as RunnerTrait, Spawner, Storage, Supervisor,
};

/// Combined Commonware runtime capability: spawning, clock, storage, and network.
pub trait Runtime: Spawner + Clock + Storage + Network + Metrics {}
impl<T: Spawner + Clock + Storage + Network + Metrics> Runtime for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokio_context_satisfies_runtime() {
        fn _accepts_runtime<R: Runtime>(_: R) {}
        fn _ctx_is_runtime(ctx: Context) {
            _accepts_runtime(ctx);
        }
        let _ = _ctx_is_runtime;
    }

    #[test]
    fn runner_is_default_constructible() {
        let _ = Runner::default();
    }
}
