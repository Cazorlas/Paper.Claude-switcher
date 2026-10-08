//! Version information. Updates come from the npm launcher (`npm/bin/cli.js`
//! handles `self-update`) or from `cargo install`; the binary never replaces
//! itself.

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
