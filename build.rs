#[cfg(feature = "sandbox-microvm")]
use std::path::Path;

#[cfg(feature = "sandbox-microvm")]
/// Search for a shared library using ldconfig and common directory paths.
/// Returns the directory containing the library if found.
fn find_library_dir(name: &str) -> Option<String> {
    // Try ldconfig -p first.
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

    // Fall back to common install paths.
    for dir in &[
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/usr/local/lib64",
    ] {
        if Path::new(dir).join(name).exists() {
            return Some(dir.to_string());
        }
    }

    None
}

fn main() {
    #[cfg(feature = "sandbox-microvm")]
    {
        // Search for both libraries before emitting any link directives.
        // If either is missing, warn and skip linking — the main binary and
        // library-level unit tests can still compile and run without libkrun.
        // The runner binary (dirge-microvm-runner) will fail to link, but
        // that's expected: it requires libkrun.so + libkrunfw.so at link time.
        // On CI, only `cargo test --bin dirge` is run, so the runner is never
        // built and the missing libkrun is not an error.
        match (
            find_library_dir("libkrun.so"),
            find_library_dir("libkrunfw.so"),
        ) {
            (Some(krun_dir), Some(krunfw_dir)) => {
                println!("cargo:rustc-link-search=native={krun_dir}");
                println!("cargo:rustc-link-lib=krun");
                if krunfw_dir != krun_dir {
                    println!("cargo:rustc-link-search=native={krunfw_dir}");
                }
                println!("cargo:rustc-link-lib=krunfw");
            }
            _ => {
                println!(
                    "cargo:warning=libkrun.so and/or libkrunfw.so not found — \
                     the microVM runner binary will not be buildable, but \
                     library-level tests and the main binary will work fine"
                );
            }
        }
    }
}
