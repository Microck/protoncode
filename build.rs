fn main() {
    let target_is_windows = std::env::var_os("CARGO_CFG_WINDOWS").is_some();
    let host_is_windows = std::env::consts::OS == "windows";
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    if target_is_windows && (host_is_windows || target_env == "gnu") {
        let mut resources = winresource::WindowsResource::new();
        resources.set_icon("assets/protoncode.ico");
        if let Err(error) = resources.compile() {
            panic!("failed to compile windows resources: {error}");
        }
    }
}
