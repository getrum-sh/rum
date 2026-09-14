//! Link against the system librpm/librpmio, but only on Linux.
//!
//! On other platforms (e.g. a macOS dev box) librpm does not exist, so we emit
//! no link directives and the crate compiles as a stub that returns an error at
//! runtime. This keeps the whole workspace buildable everywhere while the real
//! rpmdb access is exercised on the Linux test boxes.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        // rpm-devel provides the librpm.so / librpmio.so symlinks these resolve to.
        println!("cargo:rustc-link-lib=rpm");
        println!("cargo:rustc-link-lib=rpmio");
        for dir in ["/usr/lib64", "/usr/lib", "/lib64"] {
            println!("cargo:rustc-link-search=native={dir}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
