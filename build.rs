use std::path::Path;
use std::process::Command;

fn main() {
    emit_version();

    #[cfg(target_os = "windows")]
    {
        winresource::WindowsResource::new()
            .set_icon("assets/icon.ico")
            .compile()
            .expect("failed to embed icon resource");
        // If the app ever needs elevation, add assets/app.manifest and swap the
        // call above for:
        // winresource::WindowsResource::new()
        //     .set_icon("assets/icon.ico")
        //     .set_manifest_file("assets/app.manifest")
        //     .compile()
        //     .expect("failed to embed icon/manifest resources");
    }
}

/// Bakes the version the UI shows into `APP_VERSION`.
///
/// Comes from the git tag, so a release build off `v0.3.0` reads `v0.3.0` and a
/// build a few commits later reads `v0.3.0-4-g1a2b3c4`. Builds from a source
/// archive have no git data and fall back to the version in Cargo.toml.
fn emit_version() {
    let version = git_describe().unwrap_or_else(|| {
        let cargo = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
        format!("v{cargo}")
    });
    println!("cargo:rustc-env=APP_VERSION={version}");

    // Without this the version would freeze at whatever the first build saw.
    if Path::new(".git").exists() {
        println!("cargo:rerun-if-changed=.git/HEAD");
        println!("cargo:rerun-if-changed=.git/refs/tags");
    }
}

fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--dirty"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let described = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!described.is_empty()).then_some(described)
}
