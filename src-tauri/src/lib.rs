pub mod agents_sync;
pub mod bases;
pub mod commands;
pub mod config;
pub mod error;
pub mod fs_util;
pub mod git;
pub mod launcher;
pub mod sessions;
pub mod tree;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::AppState::load())
        .setup(|_app| {
            // Cópia dos agentes em segundo plano: falha só vira log.
            tauri::async_runtime::spawn_blocking(|| {
                if let Err(e) = agents_sync::sync_agents() {
                    eprintln!("brain: sincronização de agentes falhou: {e}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::scan_tree,
            commands::list_sessions,
            commands::launch,
            commands::resolve_bases,
            commands::detect_terminals,
            commands::default_config,
            commands::sync_agents,
            commands::agents_copy_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
