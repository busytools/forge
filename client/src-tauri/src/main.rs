fn main() {
    // The bundle launches without a terminal, so the plugin's log file is the
    // only artifact a failed start leaves behind.
    let run = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(tauri_plugin_log::log::LevelFilter::Info)
                .build(),
        )
        .run(tauri::generate_context!());

    if let Err(err) = run {
        tauri_plugin_log::log::error!("the client failed to start: {err}");
        eprintln!("forge client: {err}");
        std::process::exit(1);
    }
}
