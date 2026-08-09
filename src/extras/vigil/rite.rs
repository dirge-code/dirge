//! Rite gate checks — optional conditions that must pass before an observance runs.
#![allow(dead_code)]

use crate::config::VigilRite;

use super::types::RiteResult;

/// Evaluate a rite gate. If all conditions pass, returns `Pass`.
/// Currently supports:
/// - `cmd`: runs a shell command; 0 exit = pass.
/// - `git_dirty`: fails if the git working tree is dirty.
pub async fn evaluate_rite(rite: &VigilRite) -> RiteResult {
    if let Some(ref cmd) = rite.cmd
        && !cmd.is_empty()
    {
        match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .await
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let trimmed = stdout.trim().to_string();
                if !trimmed.is_empty() {
                    return RiteResult::Pass {
                        output: Some(trimmed),
                    };
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return RiteResult::Fail {
                    reason: format!(
                        "rite cmd '{cmd}' exited {}: {}",
                        output.status,
                        stderr.trim()
                    ),
                };
            }
            Err(e) => {
                return RiteResult::Fail {
                    reason: format!("rite cmd '{cmd}' failed: {e}"),
                };
            }
        }
    }

    if rite.git_dirty {
        match check_git_dirty().await {
            Ok(true) => {
                return RiteResult::Fail {
                    reason: "git working tree is dirty".to_string(),
                };
            }
            Ok(false) => {}
            Err(e) => {
                return RiteResult::Fail { reason: e };
            }
        }
    }

    RiteResult::Pass { output: None }
}

async fn check_git_dirty() -> Result<bool, String> {
    let output = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .await
        .map_err(|e| format!("git status failed: {e}"))?;
    Ok(!output.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_empty_rite_passes() {
        let rite = VigilRite::default();
        let result = evaluate_rite(&rite).await;
        assert!(matches!(result, RiteResult::Pass { .. }));
    }

    #[tokio::test]
    async fn test_rite_cmd_success_passes() {
        let rite = VigilRite {
            cmd: Some("true".to_string()),
            ..Default::default()
        };
        let result = evaluate_rite(&rite).await;
        assert!(
            matches!(result, RiteResult::Pass { .. }),
            "expected Pass but got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_rite_cmd_failure_fails() {
        let rite = VigilRite {
            cmd: Some("false".to_string()),
            ..Default::default()
        };
        let result = evaluate_rite(&rite).await;
        assert!(
            matches!(result, RiteResult::Fail { .. }),
            "expected Fail but got {:?}",
            result
        );
    }
}
