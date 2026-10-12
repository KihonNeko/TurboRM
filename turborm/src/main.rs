#![deny(unsafe_code)]

#[forbid(unsafe_code)]
mod library_state;
#[forbid(unsafe_code)]
mod library_ui;
#[forbid(unsafe_code)]
mod review_cli;
#[forbid(unsafe_code)]
mod viewer_state;

// Slint's generated toolkit bindings use unsafe internally. Hand-written modules
// forbid unsafe code; this exception applies only to generated bindings.
#[allow(unsafe_code)]
mod slint_generated {
    slint::include_modules!();
}

#[cfg(test)]
#[forbid(unsafe_code)]
mod slint_tests;

fn main() -> anyhow::Result<()> {
    // `turborm review ...` is a command-line tool: it never opens a window,
    // so it runs before any toolkit initialisation and needs no display.
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if review_cli::requested(&args) {
        std::process::exit(review_cli::main(args));
    }
    library_ui::run()
}
