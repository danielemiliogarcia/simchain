use thiserror::Error;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    #[error("missing required configuration value {key}")]
    Missing { key: &'static str },
    #[error("invalid {key}={value:?}: {cause}")]
    Invalid {
        key: &'static str,
        value: String,
        cause: String,
    },
    #[error("out-of-range {key}={value:?}: {cause}")]
    OutOfRange {
        key: &'static str,
        value: String,
        cause: String,
    },
    #[error("invalid configuration:\n{}", format_errors(.0))]
    Aggregate(Vec<ConfigError>),
}

impl ConfigError {
    pub fn invalid(key: &'static str, value: impl Into<String>, cause: impl Into<String>) -> Self {
        Self::Invalid {
            key,
            value: value.into(),
            cause: cause.into(),
        }
    }

    pub fn out_of_range(
        key: &'static str,
        value: impl Into<String>,
        cause: impl Into<String>,
    ) -> Self {
        Self::OutOfRange {
            key,
            value: value.into(),
            cause: cause.into(),
        }
    }

    pub fn aggregate(errors: impl IntoIterator<Item = ConfigError>) -> Option<Self> {
        let mut flattened = Vec::new();
        for error in errors {
            match error {
                ConfigError::Aggregate(nested) => flattened.extend(nested),
                other => flattened.push(other),
            }
        }
        match flattened.len() {
            0 => None,
            1 => flattened.into_iter().next(),
            _ => Some(ConfigError::Aggregate(flattened)),
        }
    }
}

pub fn take<T>(errors: &mut Vec<ConfigError>, result: Result<T, ConfigError>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(ConfigError::Aggregate(nested)) => {
            errors.extend(nested);
            None
        }
        Err(error) => {
            errors.push(error);
            None
        }
    }
}

pub fn finish(errors: Vec<ConfigError>) -> Result<(), ConfigError> {
    match ConfigError::aggregate(errors) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn format_errors(errors: &[ConfigError]) -> String {
    errors
        .iter()
        .map(|error| format!("- {error}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::ConfigError;

    #[test]
    fn aggregate_flattens_nested_errors() {
        let error = ConfigError::aggregate([
            ConfigError::Missing { key: "A" },
            ConfigError::Aggregate(vec![
                ConfigError::Missing { key: "B" },
                ConfigError::Missing { key: "C" },
            ]),
        ]);

        assert_eq!(
            error,
            Some(ConfigError::Aggregate(vec![
                ConfigError::Missing { key: "A" },
                ConfigError::Missing { key: "B" },
                ConfigError::Missing { key: "C" },
            ]))
        );
    }
}
