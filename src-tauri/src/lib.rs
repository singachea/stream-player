pub mod capture;
pub mod cli;
pub mod commands;
pub mod download;
pub mod error;
pub mod fetch;
pub mod hls;
pub mod player;
pub mod prefs;
pub mod proxy;
pub mod urls;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_deep_link::init())
        .manage(commands::AppState::new())
        .setup(|app| {
            capture::start(app.handle().clone());
            let handle = app.handle().clone();
            use tauri_plugin_deep_link::DeepLinkExt;
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    if let Some(c) = urls::parse_capture_url(url.as_str()) {
                        commands::ingest_capture(&handle, c);
                    }
                }
            });
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                for url in urls {
                    if let Some(c) = urls::parse_capture_url(url.as_str()) {
                        commands::ingest_capture(app.handle(), c);
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::resolve_cmd,
            commands::play_cmd,
            commands::stop_cmd,
            commands::list_streams_cmd,
            commands::stop_stream_cmd,
            commands::stream_stats_cmd,
            commands::download_cmd,
            commands::list_jobs_cmd,
            commands::cancel_job_cmd,
            commands::retry_job_cmd,
            commands::clear_jobs_cmd,
            commands::list_hosts_cmd,
            commands::save_host_cmd,
            commands::delete_host_cmd,
            commands::get_workdir_cmd,
            commands::set_workdir_cmd,
            commands::get_download_config_cmd,
            commands::set_download_config_cmd,
            commands::auto_filename_cmd,
            commands::read_clipboard_cmd,
            commands::write_clipboard_cmd,
            commands::parse_curl_cmd,
            commands::take_capture_cmd,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
