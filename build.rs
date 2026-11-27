use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.manifest");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    if target_os == "windows" && target_env == "msvc" {
        let manifest_dir = PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"),
        );
        let manifest_path = manifest_dir.join("assets").join("app.manifest");

        // Embed requireAdministrator UAC manifest only in the main GUI binary
        // so cargo test, fp-probe, and fp-testgame do not trigger UAC prompts.
        println!("cargo:rustc-link-arg-bin=framepacer-the-freak=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=framepacer-the-freak=/MANIFESTINPUT:{}",
            manifest_path.display()
        );
        println!("cargo:rustc-link-arg-bin=framepacer-the-freak=/MANIFESTUAC:NO");
    }
}
