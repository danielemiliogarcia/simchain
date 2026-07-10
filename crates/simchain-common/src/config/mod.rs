mod common;
mod env;
mod error;

pub use common::{
    CommonConfig, DEFAULT_BTC_RPC_PASS, DEFAULT_BTC_RPC_USER, DEFAULT_NODE1_RPC_URL,
    DEFAULT_NODE2_RPC_URL, DEFAULT_NODE2_WALLET_NAME, DEFAULT_NODE3_RPC_URL,
    DEFAULT_NODE3_WALLET_NAME,
};
pub use env::{
    non_empty_or, parse_bool_or, parse_bool_value, parse_optional, parse_or, parse_rpc_url,
    parse_rpc_url_or, parse_value, read, string_or, RpcUrl,
};
pub use error::{finish, take, ConfigError};
