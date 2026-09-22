pub mod commands;
pub mod engine;
pub mod history;
pub mod state;
pub mod steam_runtime;
pub mod steamkit;
pub mod store;

use std::sync::Arc;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let ctx = Arc::new(steam_runtime::AppContext::new());
    let store = match store::StoreSearch::new() {
        Ok(s) => Arc::new(s),
        Err(_) => panic!("failed to init http client"),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(ctx.clone())
        .manage(store)
        .setup(move |app| {
            *ctx.app_handle.write() = Some(app.handle().clone());
            // auto-login in the background: saved account first, else anonymous
            let ctx2 = ctx.clone();
            tauri::async_runtime::spawn(async move {
                ctx2.auto_login().await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_login_state,
            commands::login_anonymous,
            commands::login_saved,
            commands::login_password,
            commands::login_qr,
            commands::login_submit_code,
            commands::auth_poll,
            commands::auth_cancel,
            commands::logout,
            commands::search_games,
            commands::popular_games,
            commands::get_app_detail,
            commands::check_depot_access,
            commands::get_manifest_history,
            commands::start_download,
            commands::task_list,
            commands::pause_task,
            commands::resume_task,
            commands::cancel_task,
            commands::remove_task,
            commands::get_settings,
            commands::update_settings,
            commands::delete_saved_account,
            commands::app_name_hint,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
