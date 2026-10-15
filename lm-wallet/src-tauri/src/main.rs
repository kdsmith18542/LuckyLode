//! Lucky Lode Wallet — Tauri v2 shell (internal/19).

mod commands;
mod config;
mod draws;
mod emission;
mod luckynode;
mod miner;
mod node;
mod pool;

use std::collections::HashMap;
use std::sync::Mutex;
use tauri::Manager;

pub struct AppState {
    pub cfg: Mutex<config::Config>,
    pub cfg_path: std::path::PathBuf,
    pub services: Mutex<HashMap<String, luckynode::Service>>,
    pub miner: Mutex<Option<miner::Miner>>,
}

fn main() {
    tauri::Builder::default()
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
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::node_status,
            commands::wallet_balances,
            commands::transactions,
            commands::contacts_list,
            commands::contact_add,
            commands::contact_remove,
            commands::address_qr,
            commands::tx_detail,
            commands::new_address,
            commands::send,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running Lucky Lode Wallet");
}
