//! Child process lifecycle for the microVM runner.

use std::path::{Path, PathBuf};

/// Locate the `dirge-microvm-runner` binary.
pub(crate) fn find_runner_binary() -> anyhow::Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        // The test binary lives in target/debug/deps/; the runner is in
        // target/debug/. Check both the sibling dir and the grandparent.
        for candidate in &[
            exe.parent()
                .unwrap_or_else(|| Path::new("/usr/bin"))
                .join("dirge-microvm-runner"),
            exe.parent()
                .and_then(|p| p.parent())
                .unwrap_or_else(|| Path::new("/usr/bin"))
                .join("dirge-microvm-runner"),
        ] {
            if candidate.exists() {
                return Ok(candidate.clone());
            }
        }
    }

    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        let candidate = Path::new(dir).join("dirge-microvm-runner");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    anyhow::bail!("dirge-microvm-runner not found — build it and place it alongside dirge")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_runner_binary_found_in_build_tree() {
        // The runner should exist adjacent to or near the test binary
        // (e.g. target/debug/dirge-microvm-runner). Skip if not built.
        let result = find_runner_binary();
        if result.is_err() {
            eprintln!(
                "skipping: runner binary not found in build tree: {}",
                result.unwrap_err()
            );
            return;
        }
        let path = result.unwrap();
        assert!(
            path.ends_with("dirge-microvm-runner"),
            "path should end with dirge-microvm-runner, got: {}",
            path.display()
        );
    }

    #[test]
    fn find_runner_binary_from_empty_path_is_error() {
        // When the runner is NOT adjacent (no sibling binary) and PATH is
        // empty, find_runner_binary should return a clear error message.
        // If the runner happens to exist adjacent to the exe, this test
        // cannot exercise the error path and exits early.
        if find_runner_binary().is_ok() {
            // Runner found adjacent to exe — can't test the error path.
            return;
        }

        let empty_dir = std::env::temp_dir().join(format!(
            "dirge-test-runner-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&empty_dir).unwrap();

        let saved_path = std::env::var("PATH").unwrap_or_default();
        // SAFETY: set_var is unsafe due to potential thread races. Tests run
        // sequentially so no concurrent reader of PATH exists.
        unsafe { std::env::set_var("PATH", &empty_dir) };

        let result = find_runner_binary();

        unsafe { std::env::set_var("PATH", &saved_path) };
        let _ = std::fs::remove_dir_all(&empty_dir);

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("dirge-microvm-runner not found"),
            "expected 'not found' error, got: {err}"
        );
    }

    #[test]
    fn runner_stderr_captured_on_crash() {
        let binary = match find_runner_binary() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: runner binary not found: {e}");
                return;
            }
        };
        // Pass garbage JSON that will fail at the first expect().
        use std::process::Stdio;
        let output = std::process::Command::new(&binary)
            .arg("not-valid-json")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn runner");
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Verify stderr is non-empty. Before the Phase 3.1 fix,
        // Stdio::null() would have made this always empty.
        assert!(
            !stderr.is_empty(),
            "runner stderr should contain crash diagnostics"
        );
        // Should contain recognizable error text.
        assert!(
            stderr.contains("rror") || stderr.contains("sage") || stderr.contains("thread"),
            "runner stderr should contain diagnostic text, got: {stderr}"
        );
    }
}
