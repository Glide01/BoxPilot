// build.rs
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        // Embed the application icon via `app.rc`. The Windows manifest is
        // provided by gpui's bundled `gpui.manifest.xml` (its `windows-manifest`
        // feature is active transitively via gpui-component); admin elevation
        // is handled at runtime in `src/main.rs::ensure_elevated`.
        println!("cargo:rerun-if-changed=app.rc");
        println!("cargo:rerun-if-changed=assets/app.ico");
        let _ = embed_resource::compile("app.rc", embed_resource::NONE);
    }
}
