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
        fn accepts_runtime<R: Runtime>(_: R) {}
        fn ctx_is_runtime(ctx: Context) {
            accepts_runtime(ctx);
        }
        // Compile-time type-bound check only; the test body never runs it.
        let _ = ctx_is_runtime as fn(Context);
    }

    #[test]
    fn runner_is_default_constructible() {
        let _ = Runner::default();
    }
}
