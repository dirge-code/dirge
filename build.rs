#[cfg(feature = "sandbox-microvm")]
use std::path::Path;

#[cfg(feature = "sandbox-microvm")]
fn library_extension() -> &'static str {
    if cfg!(target_os = "macos") {
        ".dylib"
    } else {
        ".so"
    }
}

#[cfg(feature = "sandbox-microvm")]
/// Search for a shared library using platform-specific methods.
fn find_library_dir(name: &str) -> Option<String> {
    // On macOS, try pkg-config and Homebrew paths.
    #[cfg(target_os = "macos")]
    {
        // pkg-config metadata file is libkrun.pc with package name
        // "libkrun". The caller passes "libkrun.dylib" — strip only the
        // extension, NOT the "lib" prefix, or pkg-config always misses.
        let ext = library_extension();
        let libname = name.trim_end_matches(ext);
        if let Ok(out) = std::process::Command::new("pkg-config")
            .args(["--libs-only-L", libname])
            .output()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // pkg-config --libs-only-L may return multiple -L flags
            // (e.g. "-L/dir1 -L/dir2"). Split and take the first valid
            // one instead of blindly trimming the whole string.
            if let Some(dir) = stdout
                .split_whitespace()
                .find_map(|token| token.strip_prefix("-L"))
                .filter(|d| !d.is_empty())
            {
                return Some(dir.to_string());
            }
        }

        // Fall back to brew --prefix for the library name
        if let Ok(out) = std::process::Command::new("brew")
            .args(["--prefix"])
            .stderr(std::process::Stdio::null())
            .output()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let brew_prefix = stdout.trim();
            if !brew_prefix.is_empty() {
                let lib_dir = Path::new(brew_prefix).join("lib");
                if lib_dir.join(name).exists() {
                    return Some(lib_dir.to_string_lossy().to_string());
                }
            }
        }
    }

    // Try ldconfig -p (Linux only).
    #[cfg(target_os = "linux")]
    {
        if let Ok(out) = std::process::Command::new("ldconfig").arg("-p").output() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if line.contains(name) {
                    // ldconfig -p output format: "libkrun.so (libc6,x86-64) => /usr/lib/libkrun.so"
                    if let Some(idx) = line.find("=> ") {
                        let path = &line[idx + 3..].trim();
                        if let Some(parent) = Path::new(path).parent() {
                            return Some(parent.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }
    }

    // Fall back to common install paths.
    let ext = library_extension();
    let mut dirs: Vec<&str> = Vec::new();
    #[cfg(target_os = "macos")]
    dirs.extend(&["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"]);
    #[cfg(target_os = "linux")]
    dirs.extend(&[
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/usr/local/lib64",
    ]);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    dirs.extend(&["/usr/local/lib", "/usr/lib"]);

    for dir in dirs {
        if Path::new(dir).join(name).exists() {
            return Some(dir.to_string());
        }
    }

    // Also try the name without extension (e.g. "libkrun.dylib" → "libkrun").
    // Some platforms (macOS via brew) symlink the versioned dylib to a bare name.
    if let Some(stripped) = name.strip_suffix(ext) {
        for dir in &["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"] {
            if Path::new(dir).join(stripped).exists() {
                return Some(dir.to_string());
            }
        }
    }

    None
}

fn main() {
    #[cfg(feature = "sandbox-microvm")]
    {
        let ext = library_extension();

        // Search for both libraries before emitting any link directives.
        // If either is missing, warn and skip linking — the main binary and
        // library-level unit tests can still compile and run without libkrun.
        // The runner binary (dirge-microvm-runner) will fail to link, but
        // that's expected: it requires libkrun.so/libkrun.dylib at link time.
        // On CI, only `cargo test --bin dirge` is run, so the runner is never
        // built and the missing libkrun is not an error.
        match (
            find_library_dir(&format!("libkrun{ext}")),
            find_library_dir(&format!("libkrunfw{ext}")),
        ) {
            (Some(krun_dir), Some(krunfw_dir)) => {
                println!("cargo:rustc-link-search=native={krun_dir}");
                println!("cargo:rustc-link-lib=krun");
                if krunfw_dir != krun_dir {
                    println!("cargo:rustc-link-search=native={krunfw_dir}");
                }
                println!("cargo:rustc-link-lib=krunfw");
                // Add runtime search path so the runner can find libkrun*.dylib/.so.
                println!("cargo:rustc-link-arg-bin=dirge-microvm-runner=-Wl,-rpath,{krun_dir}");
                if krunfw_dir != krun_dir {
                    println!(
                        "cargo:rustc-link-arg-bin=dirge-microvm-runner=-Wl,-rpath,{krunfw_dir}"
                    );
                }
            }
            _ => {
                println!(
                    "cargo:warning=libkrun{ext} and/or libkrunfw{ext} not found — \
                     the microVM runner binary will not be buildable, but \
                     library-level tests and the main binary will work fine"
                );
            }
        }
    }

    // codesign is handled at runtime by ensure_runner_signed() in the sandbox
    // microvm module — no build-time wrapper needed.
    #[cfg(all(feature = "sandbox-microvm", target_os = "macos"))]
    {
        println!("cargo:rerun-if-changed=dirge.entitlements");
    }
}
