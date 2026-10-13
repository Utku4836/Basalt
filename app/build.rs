fn main() {
    println!("cargo:rerun-if-env-changed=SLINT_EMIT_DEBUG_INFO");
    let debug_info = std::env::var("PROFILE").is_ok_and(|value| value != "release")
        || std::env::var_os("SLINT_EMIT_DEBUG_INFO").is_some();
    let config = slint_build::CompilerConfiguration::new()
        .with_debug_info(debug_info);
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("Compile the desktop UI");
    // The program's own icon in Explorer and on shortcuts; the window sets its own.
    println!("cargo:rerun-if-changed=assets/icon/basalt.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resources = winresource::WindowsResource::new();
        resources.set_icon("assets/icon/basalt.ico");
        resources.compile().expect("Embed the program icon");
    }
}
