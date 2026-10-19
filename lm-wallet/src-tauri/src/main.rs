//! Lucky Lode Wallet — Tauri v2 shell.

mod commands;
mod config;
mod draws;
mod emission;
mod luckynode;
mod miner;
mod node;
mod node_proc;
mod pool;
mod wallet;

use std::collections::HashMap;
use std::sync::Mutex;
use tauri::Manager;

pub struct AppState {
    pub cfg: Mutex<config::Config>,
    pub cfg_path: std::path::PathBuf,
    pub services: Mutex<HashMap<String, luckynode::Service>>,
    pub miner: Mutex<Option<miner::Miner>>,
    /// Node daemon child this app started (None = unmanaged / not running).
    /// The wallet never supervises: no auto-restart, stop only this PID.
    pub node_proc: Mutex<Option<node_proc::ManagedNode>>,
    /// Unlocked wallet session (seed + phrase, RAM only, wiped on lock).
    /// None = locked (or no wallet yet).
    pub session: Mutex<Option<wallet::Session>>,
    /// Built-but-unbroadcast payments (txid → signed hex). Single-use,
    /// 2-minute TTL: the preview/confirm review flow can never broadcast
    /// anything but the exact bytes the user reviewed.
    pub pending: Mutex<HashMap<String, PendingSend>>,
}

/// A built + signed payment awaiting user confirmation.
pub struct PendingSend {
    pub hex: String,
    pub fee: u64,
    pub change: u64,
    pub change_used: Option<u32>,
    pub expires_at: std::time::Instant,
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let dir = app
                .path()
                .app_config_dir()
                .map_err(|e| format!("config dir: {e}"))?;
            std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
            let cfg_path = dir.join("config.json");
            let cfg = config::Config::load(&cfg_path);
            app.manage(AppState {
                cfg: Mutex::new(cfg),
                cfg_path,
                services: Mutex::new(HashMap::new()),
                miner: Mutex::new(None),
                node_proc: Mutex::new(None),
                session: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::node_status,
            commands::node_start,
            commands::node_stop,
            commands::node_discover,
            commands::use_connection_preset,
            commands::staking_info,
            commands::set_staking,
            commands::node_funding_address,
            commands::stake_app_coins,
            commands::wallet_balances,
            commands::transactions,
            commands::contacts_list,
            commands::contact_add,
            commands::contact_remove,
            commands::address_qr,
            commands::protect_address,
            commands::unprotect_address,
            commands::tx_detail,
            commands::new_address,
            commands::send,
            commands::send_preview,
            commands::send_confirm,
            commands::receive_list,
            commands::wallet_status,
            commands::wallet_create,
            commands::wallet_restore,
            commands::wallet_reveal,
            commands::wallet_rescan,
            commands::wallet_unlock,
            commands::wallet_lock,
            commands::wallet_migrate,
            commands::wallet_change_password,
            commands::game_info,
            commands::emission_info,
            commands::next_draw,
            commands::luckynodes,
            commands::pool_stats,
            commands::luckynode_register,
            commands::luckynode_service_start,
            commands::luckynode_service_stop,
            commands::luckynode_service_status,
            commands::get_config,
            commands::set_config,
            commands::set_mining_config,
            commands::test_rpc,
            commands::miner_start,
            commands::miner_stop,
            commands::miner_status,
            commands::miner_log,
            commands::miner_gpus,
            commands::notify,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Lucky Lode Wallet");
}
