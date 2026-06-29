// Link libmpv (client + render + stream_cb APIs) for the in-app video player
// (`src/mpv.rs`). We hand-roll the FFI against the system headers rather than
// pulling a crate, so all we need from the build is the link flag. pkg-config
// resolves the lib path + name (`libmpv.so.2`) portably.
fn main() {
    // Only the root binary uses libmpv; wnl-ui has its own build.rs.
    if let Err(e) = pkg_config::Config::new().atleast_version("2").probe("mpv") {
        // Fall back to a bare link directive so a missing pkg-config file
        // (but present lib) still links; surface the probe error for context.
        println!("cargo:warning=pkg-config mpv probe failed ({e}); linking -lmpv directly");
        println!("cargo:rustc-link-lib=mpv");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
