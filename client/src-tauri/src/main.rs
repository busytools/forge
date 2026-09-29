fn main() {
    // The bundle launches without a terminal, so the log file is the artifact a
    // start leaves. Its setup line is what separates "started" from "never got
    // that far", which an empty file cannot.
    let run = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(tauri_plugin_log::log::LevelFilter::Info)
                .build(),
        )
        .setup(|_app| {
            tauri_plugin_log::log::info!("forge client started");
            Ok(())
        })
        .run(tauri::generate_context!());

    if let Err(err) = run {
        tauri_plugin_log::log::error!("the client failed to start: {err}");
        eprintln!("forge client: {err}");
        std::process::exit(1);
    }
}
