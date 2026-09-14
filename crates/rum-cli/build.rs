//! Bake a human-readable version string into the binary. The Cargo version
//! (`0.1.0`) never changes across releases, so `rum --version` alone can't tell
//! which build you have. The release workflow sets `RUM_BUILD` to the release
//! tag (e.g. `v0.1.0.9`); local builds fall back to `dev`.
fn main() {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let build = std::env::var("RUM_BUILD").unwrap_or_else(|_| "dev".into());
    println!("cargo:rustc-env=RUM_VERSION_STRING={pkg} ({build})");
    println!("cargo:rerun-if-env-changed=RUM_BUILD");
}
