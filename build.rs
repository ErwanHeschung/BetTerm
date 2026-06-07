//! Build script: embed the Windows executable icon (shown in Explorer, the
//! taskbar pin, etc.) as a native resource. No-op on non-Windows targets.

fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/images/betterm.ico");
        winresource::WindowsResource::new()
            .set_icon("assets/images/betterm.ico")
            .compile()
            .expect("failed to embed Windows icon resource");
    }
}
