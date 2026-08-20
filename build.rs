// The libkrun/libkrunfw search, and the decision about what to emit from it,
// live in the crate so they can be tested — build scripts have no test
// harness, and this one previously carried a near-copy of the search that had
// drifted from the one `dirge sandbox check` uses (dirge-btpd). A build script
// can't `use` the crate it builds, so the file is included instead.
#[cfg(feature = "sandbox-microvm")]
#[allow(dead_code)]
mod libkrun_probe {
    include!("src/sandbox/libkrun_probe.rs");
}

fn main() {
    // The Mojo grammar is vendored (`grammars/tree-sitter-mojo`) rather than
    // pulled as a crate: it isn't published to crates.io under any name, and a
    // git dependency makes `cargo publish` refuse the whole package — which
    // would break the release job. Compiled here so the grammar stays inside
    // this crate. See that directory's README for provenance.
    #[cfg(feature = "semantic-mojo")]
    build_mojo_grammar();

    // Without the feature the runner isn't built at all (it declares
    // `required-features`), so there is nothing to link and nothing to probe.
    #[cfg(feature = "sandbox-microvm")]
    for directive in libkrun_probe::cargo_directives(&libkrun_probe::Probe::run()) {
        println!("{directive}");
    }

    // `cargo_directives` declares this itself; without the feature it never
    // runs, and an undeclared cfg is a lint wherever `cfg!(krun_linked)`
    // appears.
    #[cfg(not(feature = "sandbox-microvm"))]
    println!("cargo:rustc-check-cfg=cfg(krun_linked)");

    // Codesigning is handled at runtime by `ensure_runner_signed()` in the
    // sandbox microvm module, which writes its own entitlements plist to a
    // temp file. The repo's `dirge.entitlements` is used only by
    // `retry-build-microvm.sh`, and this script used to declare a
    // `rerun-if-changed` on it. That did nothing except switch off cargo's
    // default "re-run when any package file changed" heuristic, which pinned
    // the libkrun verdict for the life of the target directory (dirge-zsi8).
}

#[cfg(feature = "semantic-mojo")]
fn build_mojo_grammar() {
    let src_dir = std::path::Path::new("grammars/tree-sitter-mojo/src");

    let mut c_config = cc::Build::new();
    c_config
        .std("c11")
        .include(src_dir)
        // Upstream's own binding sets this; the generated parser has
        // unused-value warnings that are not worth patching out of a
        // regenerated file.
        .flag_if_supported("-Wno-unused-value");

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        c_config.flag("-utf-8");
    }

    for f in ["parser.c", "scanner.c"] {
        let path = src_dir.join(f);
        c_config.file(&path);
        println!("cargo:rerun-if-changed={}", path.display());
    }

    c_config.compile("tree-sitter-mojo");
}
