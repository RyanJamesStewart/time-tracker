// B4 / VBB-5: DPI manifest embedding (programmatic, no app.manifest sidecar).
//
// If you are running VBB-5 from Pane's review and `Test-Path .\app.manifest`
// returns False — that is correct and expected. This file IS the embedding.
// `embed-manifest`'s `new_manifest()` builds the manifest in-memory and links
// it into the PE; no sidecar file is needed. See Pane corpus C4.
//
//
// Per `pane-dpi-awareness-permonitorv2.md`: in MSIX, the AppxManifest
// does NOT carry the dpiAwareness setting — that lives in the side-by-side
// fusion manifest INSIDE the .exe. Without this, multi-DPI laptop+external
// setups render blurry on the external monitor (acceptance §8 line 9).
//
// `embed-manifest` is the cargo-xwin-friendly path: pure Rust, no MSVC
// resource compiler dependency. `winres` requires the RC tool that
// cargo-xwin doesn't ship — using winres here would break the
// WSL-cross-compile workflow that is the entire point of this build chain.
//
// `new_manifest()` defaults give us:
//   - dpiAwareness: PerMonitorV2, PerMonitor (graceful Win10/Win11 mix)
//   - common-controls v6
//   - longPathAware
// which is exactly the Pane invariant set.
//
// Note: `cfg(windows)` in build.rs is HOST-conditional, but this build
// runs from WSL (Linux host) targeting Windows via cargo-xwin. We
// gate on CARGO_CFG_WINDOWS instead, which Cargo sets based on the
// TARGET. Same pattern documented in pane-dpi-awareness-permonitorv2.md.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(new_manifest("RyanStewart.TimeTracker"))
            .expect("unable to embed Windows manifest");
    }
}
