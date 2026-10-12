fn main() {
    // Element debug info lets headless tests find widgets by accessible label
    // (the testing backend requires it). Release builds leave it out.
    let debug_info = std::env::var("PROFILE").as_deref() != Ok("release");
    println!("cargo:rerun-if-env-changed=PROFILE");
    let config = slint_build::CompilerConfiguration::new()
        .with_style("cosmic".into())
        .with_debug_info(debug_info);
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("Could not compile the Slint library screen");
}
