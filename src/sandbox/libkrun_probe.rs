// Discovery of the libkrun / libkrunfw shared libraries, and the `cargo:`
// directives the build script emits from it.
//
// This file is compiled twice: once as a module of the dirge crate, for
// `dirge sandbox check`, and once via `include!` from `build.rs`, for the link
// directives. A build script can't `use` the crate it builds, and the two
// copies that arrangement used to require had drifted — check.rs derived the
// pkg-config package name by stripping the `lib` prefix, so its probe never
// matched anything on macOS, while build.rs carried a comment warning about
// exactly that mistake (dirge-vij7, dirge-btpd).
//
// Two consequences of being compiled into both. Nothing here may reference the
// crate, any dependency, or any feature gate — `std` only. And the header
// above is a plain comment, not `//!`: inner attributes and inner doc comments
// can't come from an `include!` expansion, so both consumers carry the
// `#[allow(dead_code)]` on their `mod` declaration instead.
//
// `cargo_directives` lives here rather than in `build.rs` for the same reason
// the search does — build scripts have no test harness, so logic left in one
// is untested by construction.

use std::path::Path;

/// Leaf filename of the libkrun shared library on this platform.
///
/// Serves as both the link-time and the runtime name: unlike libkrunfw, the
/// name that matters is unversioned in both roles.
pub const LIBKRUN_LIB: &str = if cfg!(target_os = "macos") {
    "libkrun.dylib"
} else {
    "libkrun.so"
};

/// Leaf filename of libkrunfw as the *runtime loader* asks for it.
///
/// On macOS this is the VERSIONED name, deliberately. libkrun carries no
/// LC_RPATH and dlopens libkrunfw by bare name at runtime, and the name it
/// asks for is `libkrunfw.5.dylib` — the unversioned `libkrunfw.dylib` is a
/// symlink the loader never consults. Checking for the unversioned name would
/// report OK on a machine where the dlopen fails.
///
/// dirge-jbhz: this is a const because the string was written out in four
/// places — the check, the spawn path's existence probe, and two tests — and
/// one of the tests had drifted to the unversioned name. Since CI has no macOS
/// runner (dirge-u35k) that drift was invisible to the gate and only showed up
/// as a red suite for anyone running it on a Mac.
pub const LIBKRUNFW_LIB: &str = if cfg!(target_os = "macos") {
    "libkrunfw.5.dylib"
} else {
    "libkrunfw.so"
};

/// Leaf filename the *linker* resolves through, as distinct from
/// [`LIBKRUNFW_LIB`]: the unversioned symlink, which is what `-L<dir>` plus a
/// library name expands to at link time. The split is not drift — link time
/// and `dlopen` genuinely want different names on macOS.
pub const LIBKRUNFW_LINK_LIB: &str = if cfg!(target_os = "macos") {
    "libkrunfw.dylib"
} else {
    "libkrunfw.so"
};

/// Link-time leaf name for libkrun. Same as [`LIBKRUN_LIB`]; spelled out so
/// the build script reads symmetrically with the libkrunfw pair.
pub const LIBKRUN_LINK_LIB: &str = LIBKRUN_LIB;

/// Override for the directory holding libkrun, for installs outside the
/// prefixes [`search_dirs`] knows about.
pub const LIBKRUN_DIR_ENV: &str = "LIBKRUN_LIB_DIR";

/// Override for the directory holding libkrunfw.
pub const LIBKRUNFW_DIR_ENV: &str = "LIBKRUNFW_LIB_DIR";

/// Shared-library filename extension for this platform.
pub fn library_extension() -> &'static str {
    if cfg!(target_os = "macos") {
        ".dylib"
    } else {
        ".so"
    }
}

/// The environment variable that overrides the search for `leaf`, if any.
pub fn env_override_var(leaf: &str) -> Option<&'static str> {
    if leaf.starts_with("libkrunfw") {
        Some(LIBKRUNFW_DIR_ENV)
    } else if leaf.starts_with("libkrun") {
        Some(LIBKRUN_DIR_ENV)
    } else {
        None
    }
}

/// pkg-config package name for a shared-library leaf filename.
///
/// `libkrun.dylib` → `libkrun`, `libkrunfw.5.dylib` → `libkrunfw`,
/// `libkrunfw.so.5` → `libkrunfw`.
///
/// The `lib` prefix stays. The metadata files are `libkrun.pc` and
/// `libkrunfw.pc`, so stripping it yields `krun`, which matches no package and
/// makes the probe dead code on every machine (dirge-vij7).
pub fn pkg_config_name(leaf: &str) -> String {
    leaf.split_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(leaf)
        .to_string()
}

/// Directories searched for shared libraries, in order, after pkg-config and
/// Homebrew have had their turn.
pub fn search_dirs() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"]
    } else if cfg!(target_os = "linux") {
        &[
            "/usr/lib",
            "/usr/lib64",
            "/usr/local/lib",
            "/usr/local/lib64",
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
        ]
    } else {
        &["/usr/local/lib", "/usr/lib"]
    }
}

/// Every path the probe would consult for `leaf`, whether or not it exists.
///
/// Feeds the build script's `rerun-if-changed` set. Deliberately includes
/// paths that don't exist: cargo treats a missing watched path as changed on
/// every build, which is what makes a libkrun installed *after* a failed build
/// get noticed (dirge-zsi8).
pub fn candidate_paths(leaf: &str) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(var) = env_override_var(leaf)
        && let Some(dir) = std::env::var_os(var)
    {
        paths.push(Path::new(&dir).join(leaf).to_string_lossy().into_owned());
        // An override is exclusive — see `find_library_dir`.
        return paths;
    }
    for dir in search_dirs() {
        paths.push(Path::new(dir).join(leaf).to_string_lossy().into_owned());
    }
    paths
}

/// Directory containing `leaf`, or `None`.
///
/// Order: environment override, pkg-config, `brew --prefix`, then
/// [`search_dirs`]. An override that is set wins outright — if the library
/// isn't there, the answer is `None` rather than a silent fall back to a
/// system copy the caller didn't ask for.
pub fn find_library_dir(leaf: &str) -> Option<String> {
    if let Some(var) = env_override_var(leaf)
        && let Some(dir) = std::env::var_os(var)
    {
        let dir = Path::new(&dir);
        return dir
            .join(leaf)
            .exists()
            .then(|| dir.to_string_lossy().into_owned());
    }

    if let Some(dir) = pkg_config_dir(leaf) {
        return Some(dir);
    }

    if let Some(dir) = brew_prefix_dir(leaf) {
        return Some(dir);
    }

    if let Some(dir) = ldconfig_dir(leaf) {
        return Some(dir);
    }

    search_dirs()
        .iter()
        .find(|dir| Path::new(dir).join(leaf).exists())
        .map(|dir| (*dir).to_string())
}

/// Ask pkg-config where `leaf` lives.
///
/// `--libs-only-L` can return several `-L` flags; the one that actually holds
/// the library wins, rather than blindly taking the first.
fn pkg_config_dir(leaf: &str) -> Option<String> {
    let out = std::process::Command::new("pkg-config")
        .args(["--libs-only-L", &pkg_config_name(leaf)])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .split_whitespace()
        .filter_map(|token| token.strip_prefix("-L"))
        .filter(|dir| !dir.is_empty())
        .find(|dir| Path::new(dir).join(leaf).exists())
        .map(|dir| dir.to_string())
}

/// Look under Homebrew's prefix, wherever it is on this machine.
fn brew_prefix_dir(leaf: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("brew")
        .arg("--prefix")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let prefix = stdout.trim();
    if prefix.is_empty() {
        return None;
    }
    let lib_dir = Path::new(prefix).join("lib");
    lib_dir
        .join(leaf)
        .exists()
        .then(|| lib_dir.to_string_lossy().into_owned())
}

/// Consult the dynamic linker's cache (Linux).
fn ldconfig_dir(leaf: &str) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let out = std::process::Command::new("ldconfig")
        .arg("-p")
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if !line.contains(leaf) {
            continue;
        }
        // "libkrun.so (libc6,x86-64) => /usr/lib/libkrun.so"
        if let Some((_, path)) = line.split_once("=> ")
            && let Some(parent) = Path::new(path.trim()).parent()
        {
            return Some(parent.to_string_lossy().into_owned());
        }
    }
    None
}

/// What the probe found. Input to [`cargo_directives`].
#[derive(Debug, Default, Clone)]
pub struct Probe {
    /// Directory holding libkrun, if any. Linking hinges on this one.
    pub krun_dir: Option<String>,
    /// Directory holding libkrunfw, if any. Advisory — see
    /// [`cargo_directives`].
    pub krunfw_dir: Option<String>,
}

impl Probe {
    /// Run the search for both libraries.
    pub fn run() -> Self {
        Self {
            krun_dir: find_library_dir(LIBKRUN_LINK_LIB),
            krunfw_dir: find_library_dir(LIBKRUNFW_LINK_LIB),
        }
    }
}

/// The `cargo:` lines the build script prints for a given probe result.
///
/// Two things here are deliberate and were previously wrong:
///
/// - Only libkrun gates linking. `dirge-microvm-runner` references `krun_*`
///   and nothing else; libkrun reaches libkrunfw on its own, by `dlopen` on
///   macOS and by `DT_NEEDED` on Linux. Requiring libkrunfw before emitting
///   `-lkrun` blocked builds for no link-time reason (dirge-1158).
/// - The `rerun-if-changed` set is the library paths, not an unrelated file.
///   Emitting any `rerun-if-changed` turns off cargo's default "re-run when
///   any package file changed", so watching the wrong path pins the verdict
///   forever: install libkrun after one failed build and cargo replays the
///   cached "not found" (dirge-zsi8). When the library is missing the watched
///   paths don't exist, which cargo treats as changed every build — so the
///   probe re-runs until it finds something, then settles on the real path.
pub fn cargo_directives(probe: &Probe) -> Vec<String> {
    let mut out = Vec::new();

    // Declared unconditionally: the runner's `#[cfg(krun_linked)]` gate is
    // compiled whether or not we end up setting the cfg, and an undeclared
    // cfg trips the `unexpected_cfgs` lint.
    out.push("cargo:rustc-check-cfg=cfg(krun_linked)".to_string());

    for var in [LIBKRUN_DIR_ENV, LIBKRUNFW_DIR_ENV, "PKG_CONFIG_PATH"] {
        out.push(format!("cargo:rerun-if-env-changed={var}"));
    }

    match &probe.krun_dir {
        Some(krun_dir) => {
            out.push(format!("cargo:rustc-link-search=native={krun_dir}"));
            out.push("cargo:rustc-link-lib=krun".to_string());
            out.push(format!(
                "cargo:rustc-link-arg-bin=dirge-microvm-runner=-Wl,-rpath,{krun_dir}"
            ));
            out.push("cargo:rustc-cfg=krun_linked".to_string());
            out.push(format!(
                "cargo:rerun-if-changed={krun_dir}/{LIBKRUN_LINK_LIB}"
            ));
        }
        None => {
            out.push(format!(
                "cargo:warning={LIBKRUN_LIB} not found — skipping the microVM runner. \
                 `dirge` and every non-VM test still build. Install libkrun (macOS: \
                 brew tap libkrun/krun && brew trust libkrun/krun && brew install \
                 libkrun libkrunfw) or set {LIBKRUN_DIR_ENV}=<dir>, then rebuild."
            ));
            for path in candidate_paths(LIBKRUN_LINK_LIB) {
                out.push(format!("cargo:rerun-if-changed={path}"));
            }
        }
    }

    match &probe.krunfw_dir {
        Some(krunfw_dir) => {
            if Some(krunfw_dir) != probe.krun_dir.as_ref() {
                out.push(format!("cargo:rustc-link-search=native={krunfw_dir}"));
                out.push(format!(
                    "cargo:rustc-link-arg-bin=dirge-microvm-runner=-Wl,-rpath,{krunfw_dir}"
                ));
            }
            out.push(format!(
                "cargo:rerun-if-changed={krunfw_dir}/{LIBKRUNFW_LINK_LIB}"
            ));
        }
        None => {
            // Not fatal: the runner links without it. It is needed to boot a
            // VM, which `dirge sandbox check` reports on with the versioned
            // name the loader actually asks for.
            out.push(format!(
                "cargo:warning={LIBKRUNFW_LINK_LIB} not found — the runner will link, \
                 but booting a microVM needs it at runtime. Run `dirge sandbox check`."
            ));
            for path in candidate_paths(LIBKRUNFW_LINK_LIB) {
                out.push(format!("cargo:rerun-if-changed={path}"));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(krun: Option<&str>, krunfw: Option<&str>) -> Probe {
        Probe {
            krun_dir: krun.map(String::from),
            krunfw_dir: krunfw.map(String::from),
        }
    }

    /// The bug that made check.rs's pkg-config probe dead code: the metadata
    /// files are `libkrun.pc` / `libkrunfw.pc`, so the `lib` prefix is part of
    /// the package name and only the extension comes off (dirge-vij7).
    #[test]
    fn pkg_config_name_keeps_the_lib_prefix() {
        assert_eq!(pkg_config_name("libkrun.dylib"), "libkrun");
        assert_eq!(pkg_config_name("libkrun.so"), "libkrun");
        assert_eq!(pkg_config_name("libkrunfw.dylib"), "libkrunfw");
        assert_eq!(pkg_config_name("libkrunfw.so"), "libkrunfw");
    }

    /// The runtime name is versioned on macOS and the soname is versioned on
    /// Linux; neither spelling is a different package.
    #[test]
    fn pkg_config_name_drops_version_suffixes() {
        assert_eq!(pkg_config_name("libkrunfw.5.dylib"), "libkrunfw");
        assert_eq!(pkg_config_name("libkrunfw.so.5"), "libkrunfw");
    }

    #[test]
    fn env_override_var_distinguishes_the_two_libraries() {
        assert_eq!(env_override_var("libkrun.dylib"), Some(LIBKRUN_DIR_ENV));
        assert_eq!(env_override_var("libkrun.so"), Some(LIBKRUN_DIR_ENV));
        // Must not fall to the libkrun arm on a prefix match.
        assert_eq!(
            env_override_var("libkrunfw.5.dylib"),
            Some(LIBKRUNFW_DIR_ENV)
        );
        assert_eq!(env_override_var("libkrunfw.so"), Some(LIBKRUNFW_DIR_ENV));
        assert_eq!(env_override_var("libssl.so"), None);
    }

    /// The whole point of the rewrite: the watched set is the thing whose
    /// arrival changes the answer. Watching anything else pins the verdict
    /// forever, because emitting *any* `rerun-if-changed` disables cargo's
    /// default "re-run when a package file changed" (dirge-zsi8).
    #[test]
    fn missing_libkrun_watches_the_paths_that_would_supply_it() {
        let directives = cargo_directives(&probe(None, None));
        let watched: Vec<_> = directives
            .iter()
            .filter_map(|d| d.strip_prefix("cargo:rerun-if-changed="))
            .collect();
        for dir in search_dirs() {
            let expected = format!("{dir}/{LIBKRUN_LINK_LIB}");
            assert!(
                watched.contains(&expected.as_str()),
                "not watching {expected}; watched: {watched:?}"
            );
        }
    }

    /// `dirge.entitlements` is never read by the build script — the runner
    /// signs itself at runtime with a plist it writes to a temp file. Watching
    /// it did nothing except switch off the default re-run heuristic.
    #[test]
    fn nothing_watches_the_entitlements_file() {
        for p in [probe(None, None), probe(Some("/opt/homebrew/lib"), None)] {
            for directive in cargo_directives(&p) {
                assert!(
                    !directive.contains("entitlements"),
                    "build script still watches the entitlements file: {directive}"
                );
            }
        }
    }

    #[test]
    fn found_libkrun_emits_link_directives_and_the_cfg() {
        let directives = cargo_directives(&probe(Some("/opt/homebrew/lib"), None));
        assert!(directives.contains(&"cargo:rustc-link-lib=krun".to_string()));
        assert!(directives.contains(&"cargo:rustc-cfg=krun_linked".to_string()));
        assert!(
            directives.contains(&"cargo:rustc-link-search=native=/opt/homebrew/lib".to_string())
        );
        assert!(
            directives.contains(
                &"cargo:rustc-link-arg-bin=dirge-microvm-runner=-Wl,-rpath,/opt/homebrew/lib"
                    .to_string()
            )
        );
    }

    /// Nothing in the runner references a `krunfw_*` symbol — libkrun reaches
    /// libkrunfw itself. Gating `-lkrun` on libkrunfw blocked the build for no
    /// link-time reason (dirge-1158).
    #[test]
    fn libkrunfw_absence_does_not_block_linking() {
        let directives = cargo_directives(&probe(Some("/opt/homebrew/lib"), None));
        assert!(
            directives.contains(&"cargo:rustc-cfg=krun_linked".to_string()),
            "libkrunfw missing should still link libkrun: {directives:?}"
        );
        assert!(
            !directives
                .iter()
                .any(|d| d == "cargo:rustc-link-lib=krunfw"),
            "should not link against libkrunfw at all: {directives:?}"
        );
    }

    /// The converse, and the case that actually failed on the reporting
    /// machine: no libkrun means no `krun_linked`, so the runner compiles to
    /// its stub instead of failing to link (dirge-vadg).
    #[test]
    fn missing_libkrun_emits_no_link_directives() {
        let directives = cargo_directives(&probe(None, Some("/opt/homebrew/lib")));
        assert!(
            !directives
                .iter()
                .any(|d| d.starts_with("cargo:rustc-link-lib")),
            "{directives:?}"
        );
        assert!(
            !directives.contains(&"cargo:rustc-cfg=krun_linked".to_string()),
            "{directives:?}"
        );
        assert!(
            directives
                .iter()
                .any(|d| d.starts_with("cargo:warning=") && d.contains(LIBKRUN_LIB)),
            "should say which library is missing: {directives:?}"
        );
    }

    /// Without this the `#[cfg(krun_linked)]` gate in the runner draws an
    /// `unexpected_cfgs` warning on every build, found or not.
    #[test]
    fn the_cfg_is_always_declared() {
        for p in [probe(None, None), probe(Some("/lib"), Some("/lib"))] {
            assert!(
                cargo_directives(&p)
                    .contains(&"cargo:rustc-check-cfg=cfg(krun_linked)".to_string()),
                "check-cfg must be emitted regardless of what was found"
            );
        }
    }

    /// An override is useless if changing it doesn't re-run the probe — and
    /// with the default heuristic off, only `rerun-if-env-changed` does that
    /// (dirge-fp3l).
    #[test]
    fn override_variables_retrigger_the_probe() {
        let directives = cargo_directives(&probe(Some("/lib"), Some("/lib")));
        for var in [LIBKRUN_DIR_ENV, LIBKRUNFW_DIR_ENV, "PKG_CONFIG_PATH"] {
            assert!(
                directives.contains(&format!("cargo:rerun-if-env-changed={var}")),
                "{var} should retrigger the probe: {directives:?}"
            );
        }
    }

    /// A shared directory must not produce duplicate `-L`/`-rpath` flags.
    #[test]
    fn one_directory_is_not_searched_twice() {
        let directives = cargo_directives(&probe(Some("/lib"), Some("/lib")));
        let searches = directives
            .iter()
            .filter(|d| d.starts_with("cargo:rustc-link-search="))
            .count();
        assert_eq!(searches, 1, "{directives:?}");
    }

    /// Pins the decision the consts encode. libkrun dlopens libkrunfw by bare
    /// VERSIONED name, so the runtime check must use that; the linker resolves
    /// through the unversioned symlink, so link directives must use that. The
    /// two differing is correct, and each being wrong is silent (dirge-jbhz).
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_splits_the_runtime_and_link_names() {
        assert_eq!(LIBKRUN_LIB, "libkrun.dylib");
        assert_eq!(LIBKRUNFW_LIB, "libkrunfw.5.dylib");
        assert_eq!(LIBKRUNFW_LINK_LIB, "libkrunfw.dylib");
        assert_eq!(library_extension(), ".dylib");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_uses_one_name_for_both_roles() {
        assert_eq!(LIBKRUN_LIB, "libkrun.so");
        assert_eq!(LIBKRUNFW_LIB, "libkrunfw.so");
        assert_eq!(LIBKRUNFW_LINK_LIB, "libkrunfw.so");
        assert_eq!(library_extension(), ".so");
    }
}
