use super::ConfigError;
use std::{env, fmt, str::FromStr};
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcUrl(String);

impl RpcUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for RpcUrl {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for RpcUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn read(key: &'static str) -> Option<String> {
    env::var(key).ok()
}

pub fn string_or(key: &'static str, default: &'static str) -> String {
    read(key).unwrap_or_else(|| default.to_string())
}

pub fn non_empty_or(key: &'static str, default: &'static str) -> Result<String, ConfigError> {
    let value = string_or(key, default);
    if value.trim().is_empty() {
        return Err(ConfigError::invalid(key, value, "value must not be empty"));
    }
    Ok(value)
}

pub fn parse_value<T>(key: &'static str, value: impl Into<String>) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    let value = value.into();
    value
        .trim()
        .parse::<T>()
        .map_err(|error: T::Err| ConfigError::invalid(key, value, error.to_string()))
}

pub fn parse_or<T>(key: &'static str, default: &'static str) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    parse_value(key, string_or(key, default))
}

pub fn parse_optional<T>(key: &'static str) -> Result<Option<T>, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match read(key) {
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) => parse_value(key, value).map(Some),
        None => Ok(None),
    }
}

pub fn parse_bool_value(key: &'static str, value: impl Into<String>) -> Result<bool, ConfigError> {
    let value = value.into();
    match value.trim() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ConfigError::invalid(
            key,
            value,
            "expected one of: true, false, 1, 0",
        )),
    }
}

pub fn parse_bool_or(key: &'static str, default: &'static str) -> Result<bool, ConfigError> {
    parse_bool_value(key, string_or(key, default))
}

pub fn parse_rpc_url_or(key: &'static str, default: &'static str) -> Result<RpcUrl, ConfigError> {
    parse_rpc_url(key, string_or(key, default))
}

pub fn parse_rpc_url(key: &'static str, value: impl Into<String>) -> Result<RpcUrl, ConfigError> {
    let value = value.into();
    Url::parse(&value)
        .map_err(|error| ConfigError::invalid(key, value.clone(), error.to_string()))?;
    Ok(RpcUrl(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_or_returns_default_when_unset() {
        assert_eq!(
            string_or("SIMCHAIN_COMMON_DEFINITELY_UNSET_KEY", "fallback"),
            "fallback"
        );
    }

    #[test]
    fn string_or_returns_value_when_set() {
        env::set_var("SIMCHAIN_COMMON_SET_KEY", "actual");
        assert_eq!(string_or("SIMCHAIN_COMMON_SET_KEY", "fallback"), "actual");
        env::remove_var("SIMCHAIN_COMMON_SET_KEY");
    }

    #[test]
    fn parse_bool_value_accepts_supported_forms() {
        assert!(parse_bool_value("BOOL", "true").unwrap_or(false));
        assert!(parse_bool_value("BOOL", "1").unwrap_or(false));
        assert!(!parse_bool_value("BOOL", "false").unwrap_or(true));
        assert!(!parse_bool_value("BOOL", "0").unwrap_or(true));
    }

    #[test]
    fn parse_bool_value_rejects_other_values() {
        assert!(matches!(
            parse_bool_value("BOOL", "yes"),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn parse_optional_treats_empty_as_unset() {
        env::set_var("SIMCHAIN_COMMON_OPTIONAL_KEY", "");
        let parsed = parse_optional::<u64>("SIMCHAIN_COMMON_OPTIONAL_KEY").unwrap_or(None);
        env::remove_var("SIMCHAIN_COMMON_OPTIONAL_KEY");

        assert_eq!(parsed, None);
    }

    #[test]
    fn parse_rpc_url_validates_urls() {
        let valid = parse_rpc_url("RPC_URL", "http://example.com:18443")
            .unwrap_or_else(|_| RpcUrl("http://invalid".to_string()));
        assert_eq!(valid.as_str(), "http://example.com:18443");
        assert!(matches!(
            parse_rpc_url("RPC_URL", "not a url"),
            Err(ConfigError::Invalid { .. })
        ));
    }
}
