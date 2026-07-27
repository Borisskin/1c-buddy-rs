const WINDOWS_ICON: &str = "assets/onec-buddy.ico";

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed={WINDOWS_ICON}");
    println!("cargo:rerun-if-changed=Cargo.toml");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    winresource::WindowsResource::new()
        .set_icon(WINDOWS_ICON)
        .compile()
}
