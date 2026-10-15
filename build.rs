
// Copy the runtime artifacts next to the built exe so a built tree is
// deployable as-is.
//
// These are third-party binaries (see driver/PAWNIO-MODULES-LICENSE.txt and
// refs/vendor/OlsIoctl.h) and are intentionally NOT tracked in git. They are
// shipped inside the repo's `driver/` dir when present; if a fresh clone has
// none, `tools/fetch-driver-assets.ps1` downloads them. A missing file is a
// warning here, not a hard error: PawnIO-only users need no .sys, and a
// missing module is reported loudly at runtime with the reason.
fn deploy_runtime_assets() {
    let Some(out_dir) = std::env::var_os("OUT_DIR") else {
        return;
    };
    // target/<profile>/build/<pkg>-<hash>/out  ->  target/<profile>
    let out = std::path::PathBuf::from(out_dir);
    let Some(profile_dir) = out.ancestors().nth(3) else {
        return;
    };
    let files = ["WinRing0x64.sys", "LpcIO.bin", "IntelMSR.bin"];
    let mut missing = Vec::new();
    for f in files {
        let src = std::path::Path::new("driver").join(f);
        if !src.exists() {
            missing.push(f);
            continue;
        }
        if let Err(e) = std::fs::copy(&src, profile_dir.join(f)) {
            println!("cargo:warning=could not copy {f} to {}: {e}", profile_dir.display());
        }
    }
    if !missing.is_empty() {
        println!(
            "cargo:warning=driver assets missing: {}. Run tools/fetch-driver-assets.ps1 \
             (PawnIO modules are required; WinRing0x64.sys only for the WinRing0 backend).",
            missing.join(", ")
        );
    }
}
fn main() {
    // NOTE on the ".rsrc merge failure: multiple non-default manifests" linker
    // warning this build produces: gcc unconditionally appends its own
    // `default-manifest.o` (an asInvoker manifest) to the link line, so ld finds
    // two RT_MANIFEST resources in .rsrc. It keeps OURS — verified by reading the
    // RT_MANIFEST resource back out of the built exe and seeing requireAdministrator
    // — so the warning is noise, not a broken build. No ld flag drops a named input
    // object, and that object is appended after every `-l`, so it cannot be
    // avoided from here either.
    //
    // `update_manifest.ps1` is therefore still required after every build: it
    // re-embeds the manifest through the Win32 UpdateResource API, which also
    // keeps the exe correct if a toolchain ever changes which manifest wins.
    // Do not "fix" the warning by deleting the resource below.
    //
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
    deploy_runtime_assets();
}

