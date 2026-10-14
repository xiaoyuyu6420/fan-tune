fn main() {
    // On GNU toolchain we use windres + ar to embed the requireAdministrator
    // manifest (app.rc -> app.manifest) as a PE resource.
    //
    // The resource goes into a static archive that main.rs links only when it
    // is NOT compiling the `cargo test` harness (`#[cfg(not(test))]`). A plain
    // `cargo:rustc-link-arg` (or `-bins`) would also put the manifest into the
    // test harness, and a binary that requests administrator rights cannot be
    // started from a normal shell at all (os error 740).
    let rc = std::path::Path::new("src/app.rc");
    if rc.exists() {
        println!("cargo:rerun-if-changed=src/app.rc");
        println!("cargo:rerun-if-changed=src/app.manifest");
        // windres ships with mingw64 gcc, ar with binutils
        let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "target".into());
        let obj = format!("{out_dir}/app_resource.o");
        let status = std::process::Command::new("windres")
            .args(&["--input", "src/app.rc", "--output", &obj])
            .status();
        if let Ok(s) = status {
            if s.success() {
                let lib = format!("{out_dir}/libapp_resource.a");
                let packed = std::process::Command::new("ar")
                    .args(["crs", &lib, &obj])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if packed {
                    println!("cargo:rustc-link-search=native={out_dir}");
                } else {
                    // The exe would silently lose its elevation requirement —
                    // fail instead of shipping a build that cannot reach the
                    // kernel driver.
                    panic!("ar not found: windres produced {obj} but it could not be archived");
                }
            }
        }
    }
}
