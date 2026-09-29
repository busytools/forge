fn main() {
    if let Err(err) = tauri::Builder::default().run(tauri::generate_context!()) {
        eprintln!("forge client: {err}");
        std::process::exit(1);
    }
}
