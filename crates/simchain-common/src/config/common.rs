use super::{finish, non_empty_or, take, ConfigError};
use crate::rpc::RPC_TIMEOUT_SECS;
use std::{sync::OnceLock, time::Duration};

pub const DEFAULT_BTC_RPC_USER: &str = "foo";
pub const DEFAULT_BTC_RPC_PASS: &str = "rpcpassword";
pub const DEFAULT_NODE1_RPC_URL: &str = "http://btc-simnet-node1:18443";
pub const DEFAULT_NODE2_RPC_URL: &str = "http://btc-simnet-node2:18443";
pub const DEFAULT_NODE3_RPC_URL: &str = "http://btc-simnet-node3:18443";
pub const DEFAULT_NODE2_WALLET_NAME: &str = "node2";
pub const DEFAULT_NODE3_WALLET_NAME: &str = "node3";

static COMMON_CONFIG: OnceLock<CommonConfig> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct CommonConfig {
    pub rpc_user: String,
    pub rpc_pass: String,
    pub rpc_timeout: Duration,
}

impl CommonConfig {
    pub fn init() -> Result<&'static Self, ConfigError> {
        if let Some(config) = COMMON_CONFIG.get() {
            return Ok(config);
        }

        let config = Self::from_env()?;
        Ok(Self::install(config))
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        let mut errors = Vec::new();
        let rpc_user = take(
            &mut errors,
            non_empty_or("BTC_RPC_USER", DEFAULT_BTC_RPC_USER),
        );
        let rpc_pass = take(
            &mut errors,
            non_empty_or("BTC_RPC_PASS", DEFAULT_BTC_RPC_PASS),
        );

        finish(errors)?;

        let (Some(rpc_user), Some(rpc_pass)) = (rpc_user, rpc_pass) else {
            unreachable!("CommonConfig fields must be present after validation");
        };

        Ok(Self {
            rpc_user,
            rpc_pass,
            rpc_timeout: Duration::from_secs(RPC_TIMEOUT_SECS),
        })
    }

    pub fn init_with<T>(tool: Result<T, ConfigError>) -> Result<T, ConfigError> {
        if COMMON_CONFIG.get().is_some() {
            return tool;
        }

        let common = Self::from_env();
        let (common, tool) = combine(common, tool)?;
        Self::install(common);
        Ok(tool)
    }

    pub fn install(config: Self) -> &'static Self {
        let _ = COMMON_CONFIG.set(config);
        Self::global()
    }

    pub fn global() -> &'static Self {
        COMMON_CONFIG
            .get()
            .unwrap_or_else(|| panic!("CommonConfig::init() not called in main"))
    }
}

fn combine<T, U>(
    left: Result<T, ConfigError>,
    right: Result<U, ConfigError>,
) -> Result<(T, U), ConfigError> {
    match (left, right) {
        (Ok(left), Ok(right)) => Ok((left, right)),
        (left, right) => {
            let mut errors = Vec::new();
            if let Err(error) = left {
                errors.push(error);
            }
            if let Err(error) = right {
                errors.push(error);
            }
            if let Some(error) = ConfigError::aggregate(errors) {
                Err(error)
            } else {
                unreachable!("at least one config error must be present");
            }
        }
    }
}
