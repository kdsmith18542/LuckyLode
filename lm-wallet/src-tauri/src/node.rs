//! Node JSON-RPC helpers (reusing the pool's std-only RpcClient).

use crate::AppState;
use lm_stratum::rpc::RpcClient;

/// RPC client from the current settings (`rpc_url`; `http(s)://` optional,
/// path prefixes are honoured so a reverse-proxied public node works —
/// e.g. `https://luckylode.cc/node/`).
pub fn client(state: &AppState) -> Result<RpcClient, String> {
    let cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let url = cfg.rpc_url.trim();
    if url.is_empty() {
        return Err("node RPC URL is empty (Settings)".to_string());
    }
    let url = if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{url}")
    };
    RpcClient::from_url(&url, &cfg.rpc_user, &cfg.rpc_pass)
}

/// Call and return the JSON `result`, with a friendlier auth error.
pub fn call(
    state: &AppState,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    client(state)?.call(method, params).map_err(|e| {
        if e.contains("401") || e.contains("403") {
            format!(
                "node rejected the RPC login ({e}) — set the RPC user/password in Settings (they must match the node's rpcuser/rpcpassword)"
            )
        } else {
            e
        }
    })
}
