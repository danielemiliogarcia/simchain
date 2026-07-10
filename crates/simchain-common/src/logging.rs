//! Shared tracing setup for the simchain binaries.

/// Initialize the standard simchain tracing subscriber. `default_filter` is
/// tool-specific so each binary retains a useful crate-target default while
/// honoring `RUST_LOG` when the caller provides one.
pub fn init_tracing(default_filter: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.parse().unwrap()),
        )
        .init();
}
