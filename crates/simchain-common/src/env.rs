//! Environment lookup.

use std::env;

/// Read `key` from the environment, falling back to `default` when it is unset.
pub fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_or_returns_default_when_unset() {
        assert_eq!(
            env_or("SIMCHAIN_COMMON_DEFINITELY_UNSET_KEY", "fallback"),
            "fallback"
        );
    }

    #[test]
    fn env_or_returns_value_when_set() {
        // Unique key so this cannot collide with the unset-key test even if the
        // suite is ever run multi-threaded.
        env::set_var("SIMCHAIN_COMMON_SET_KEY", "actual");
        assert_eq!(env_or("SIMCHAIN_COMMON_SET_KEY", "fallback"), "actual");
        env::remove_var("SIMCHAIN_COMMON_SET_KEY");
    }
}
