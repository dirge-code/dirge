//! Cheap-model digest of large subagent results.
//!
//! A subagent's final answer above the `task` inline budget used to reach
//! the parent as a head/tail excerpt plus a path to the full text
//! ([`relay_if_large`]). The parent then either worked from a clipped middle
//! or spent its own (expensive) tokens reading the whole file.
//!
//! With `subagent_digest_provider` set, a cheap model reads the full text
//! once and writes a digest; the parent gets the digest plus the same path.
//! The full text is still on disk, and the user still sees it verbatim in
//! the subagent's chat tab. Any failure (no model, timeout, provider error,
//! empty digest) falls back to the excerpt, so digesting can only add
//! information, never lose the result.

use std::sync::Mutex;
use std::time::Duration;

use crate::agent::tools::output_relay::relay_if_large;
use crate::provider::AnyModel;
use crate::sync_util::LockExt;

/// The model that writes digests, installed by each agent build from
/// `subagent_digest_provider`. `None` disables digesting.
static DIGEST_MODEL: Mutex<Option<AnyModel>> = Mutex::new(None);

/// Upper bound on one digest call. A digest that is slower than this is not
/// worth waiting for: the excerpt is delivered instead.
const DIGEST_TIMEOUT: Duration = Duration::from_secs(90);

const DIGEST_PREAMBLE: &str = "You condense a coding subagent's final report for the \
agent that dispatched it. That agent will act on your digest without reading the report, \
so keep every concrete fact it could need: file paths with line numbers, symbol names, \
commands and their results, findings, decisions, errors, and open questions. Drop \
narration, repetition and hedging. Do not add anything the report does not say. Use terse \
bullet points under short headings. Stay under 400 words.";

/// Install (or clear, with `None`) the digest model.
pub fn set_digest_model(model: Option<AnyModel>) {
    *DIGEST_MODEL.lock_ignore_poison() = model;
}

fn digest_model() -> Option<AnyModel> {
    DIGEST_MODEL.lock_ignore_poison().clone()
}

/// Digest a subagent's final text for its parent when it is over the `task`
/// inline budget and a digest model is configured. The full text is written
/// to the relay file first, so the digest can point at it. `None` means
/// "deliver as before": the text fits inline, no model is set, or the digest
/// call failed (logged) - the caller's existing relay path then applies.
pub async fn try_digest(text: &str) -> Option<String> {
    let model = digest_model()?;
    let path = relay_if_large("task", text.to_string(), "").relayed_to?;
    let call = model.btw_query_with(digest_prompt(text), Some(DIGEST_PREAMBLE));
    let why = match tokio::time::timeout(DIGEST_TIMEOUT, call).await {
        Ok(Ok(digest)) if !digest.trim().is_empty() => {
            return Some(format_digest(
                &model.name(),
                text.len(),
                &path,
                digest.trim(),
            ));
        }
        Ok(Ok(_)) => "digest model returned nothing".to_string(),
        Ok(Err(e)) => e.to_string(),
        Err(_) => "digest timed out".to_string(),
    };
    tracing::warn!(target: "dirge::subagents", "subagent digest skipped: {why}");
    None
}

fn digest_prompt(full: &str) -> String {
    format!("<subagent_report>\n{full}\n</subagent_report>")
}

fn format_digest(model: &str, bytes: usize, path: &std::path::Path, digest: &str) -> String {
    format!(
        "[subagent result digested by {model} from {bytes} bytes. Full text: {}. Read it \
         (with offset/limit) only if the digest lacks something you need.]\n\n{digest}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_header_names_model_size_and_path() {
        let out = format_digest(
            "deepseek-chat",
            20_000,
            std::path::Path::new("/tmp/task-1.txt"),
            "- found it",
        );
        assert!(out.starts_with("[subagent result digested by deepseek-chat from 20000 bytes."));
        assert!(out.contains("/tmp/task-1.txt"));
        assert!(out.ends_with("\n\n- found it"));
    }

    #[tokio::test]
    async fn small_results_are_not_digested() {
        assert_eq!(try_digest("done").await, None);
    }

    #[test]
    fn prompt_wraps_the_full_report() {
        let p = digest_prompt("line one\nline two");
        assert!(p.starts_with("<subagent_report>\nline one"));
        assert!(p.ends_with("line two\n</subagent_report>"));
    }
}
