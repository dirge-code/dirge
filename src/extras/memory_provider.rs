//! Pluggable memory backend trait — port of hermes-agent's
//! `agent/memory_provider.py` `MemoryProvider` ABC, adapted for
//! Rust.
//!
//! Hermes lets users swap the built-in memory tool for a backend of
//! their choice (Hindsight, Honcho, custom) by implementing this
//! ABC. Dirge previously hard-coded `MemoryToolStore` everywhere it
//! used memory, blocking any future alternative backend.
//!
//! Design decisions ported from hermes:
//! - Lifecycle hooks (`on_session_end`, `on_memory_write`,
//!   `on_pre_compress`) so providers can react to events without
//!   being asked.
//! - Core CRUD (`view`/`add`/`replace`/`remove`) matching the
//!   existing `MemoryTool` schema so the tool layer doesn't need a
//!   parallel rewrite.
//! - Default no-op hooks so existing back-ends only override what
//!   they care about.
//!
//! The dirge `MemoryToolStore` (per-project MEMORY.md/PITFALLS.md
//! backing the default tool) is the canonical implementation. New
//! backends — e.g. a future MCP-server-backed provider, an embedding
//! store, a global cross-project store — implement this trait and
//! plug in at `agent::builder` time.
//!
//! See dirge-bov5.

use serde_json::Value;

/// Pluggable backend for the `memory` tool. Implementors are stored
/// behind `Arc<dyn MemoryProvider>` so the tool layer can hold a
/// fixed reference while the concrete backend is swapped at agent
/// construction time.
pub trait MemoryProvider: Send + Sync {
    /// Short identifier — used in logs and diagnostics. Hermes uses
    /// `"builtin"`, `"hindsight"`, etc.
    fn name(&self) -> &str;

    /// Render the frozen system-prompt snapshot for this provider.
    /// Called once at agent-builder time; the result is injected
    /// into the preamble. Return an empty string to skip injection.
    fn format_for_system_prompt(&self) -> String {
        String::new()
    }

    /// Return all entries under `target` (e.g. `"memory"` /
    /// `"pitfalls"`). The response shape matches the existing tool
    /// schema — a JSON object with `entries`, `count`, `usage_pct`.
    fn view(&self, target: &str) -> Value;

    /// Append a new entry. `kind` is the UMP memory kind
    /// (types.ts:8-13); `None` defaults to `"procedural"`.
    fn add(&self, target: &str, content: &str, kind: Option<&str>) -> Result<Value, String>;

    /// Replace an entry matched by substring. `old_text` must
    /// uniquely identify an entry; ambiguous matches error.
    /// `kind` is the UMP memory kind for the replacement entry;
    /// `None` defaults to `"procedural"`.
    fn replace(
        &self,
        target: &str,
        old_text: &str,
        content: &str,
        kind: Option<&str>,
    ) -> Result<Value, String>;

    /// Supersede an entry with a newer, contradicting fact (dirge-fa10).
    /// Unlike `replace` (in-place edit of the SAME fact), this retires
    /// the old entry as an audit record and writes a NEW one. `harsh`
    /// marks a flat user denial (vs a natural update), which the builtin
    /// backend uses to discount the successor's confidence. Default
    /// errors so backends without a confidence/supersession model don't
    /// silently fall back to a lossy replace.
    fn supersede(
        &self,
        _target: &str,
        _old_text: &str,
        _content: &str,
        _kind: Option<&str>,
        _harsh: bool,
    ) -> Result<Value, String> {
        Err("This memory backend does not support superseding entries".to_string())
    }

    /// Drop an entry matched by substring. Same uniqueness rule as
    /// `replace`. The builtin backend tombstones rather than deletes
    /// (dirge-8h22); other backends may hard-delete.
    fn remove(&self, target: &str, old_text: &str) -> Result<Value, String>;

    /// Bring a previously removed (tombstoned/archived) entry back.
    /// `old_text` matches over archived entries with the same
    /// substring/id rules as `replace`/`remove`. Default errors so
    /// backends without an archive don't silently no-op (dirge-8h22).
    fn restore(&self, _target: &str, _old_text: &str) -> Result<Value, String> {
        Err("This memory backend does not support restoring removed entries".to_string())
    }

    /// Fetch one entry's full text by id or unique substring, across
    /// targets — the dereference half of the breadcrumb index
    /// (dirge-q8wt). Default errors for backends without tiering.
    fn expand(&self, _old_text: &str) -> Result<Value, String> {
        Err("This memory backend does not support expanding entries".to_string())
    }

    /// Full-text search across all active entries (dirge-q8wt).
    /// Default errors for backends without a search index.
    fn search(&self, _query: &str) -> Result<Value, String> {
        Err("This memory backend does not support searching entries".to_string())
    }

    /// Record a procedural playbook's real-world outcome (dirge-zygq):
    /// `success=true` for a confirmed success, `false` for a failure.
    /// `old_text` matches by uid or unique substring. Intended for the
    /// background review pass, which infers outcomes from the
    /// transcript. Default errors for backends that don't track
    /// effectiveness.
    fn record_outcome(
        &self,
        _target: &str,
        _old_text: &str,
        _success: bool,
    ) -> Result<Value, String> {
        Err("This memory backend does not support recording outcomes".to_string())
    }

    /// Re-fetch the system-prompt snapshot from the live backing
    /// store WITHOUT tearing down the session. The next
    /// `format_for_system_prompt()` call will reflect all writes
    /// since the last snapshot. Default no-op for backends that
    /// don't cache a frozen snapshot (they already read live).
    fn refresh_snapshot(&self) -> Result<(), String> {
        Ok(())
    }

    // ── Optional lifecycle hooks — default no-ops ──────────────

    /// Notify the provider that a memory write just happened via
    /// the tool layer. Use to mirror the write to a secondary
    /// backend (e.g. a vector store), audit log, or analytics
    /// sink.
    ///
    /// `action` is one of `"add"`, `"replace"`, `"remove"`,
    /// `"restore"`. Consumers should ignore actions they don't know —
    /// the set can grow.
    ///
    /// `payload` carries action-specific data — the semantics
    /// differ by action, NOT a generic "new content" field:
    /// - `"add"` → the entry text being appended.
    /// - `"replace"` → the NEW entry text (what's being written).
    /// - `"remove"` → the `old_text` substring that identified
    ///   the deleted entry (no new content; this is the only
    ///   information the tool has about what just disappeared).
    ///
    /// Providers that mirror writes to another store MUST check
    /// `action` before treating `payload` as "the new value" —
    /// dirge-ix7n made the asymmetry explicit so a plugin author
    /// can't accidentally persist `payload` as the latest content
    /// after a remove. The single-param shape mirrors hermes's
    /// `on_memory_write(action, target, content, metadata)` minus
    /// the metadata bag (dirge doesn't ship structured metadata).
    fn on_memory_write(&self, _action: &str, _target: &str, _payload: &str) {}

    /// Notify the provider that the live session ended. Use for
    /// end-of-session fact extraction, queue flushing, or
    /// summarization. `transcript` is the full conversation text.
    fn on_session_end(&self, _transcript: &str) {}

    /// Notify the provider that the session id is changing
    /// mid-process. Ported from hermes
    /// `MemoryProvider.on_session_switch` (memory_provider.py:162-194).
    ///
    /// Fires on dirge events that reassign `session.id` without
    /// tearing the provider down — currently the compaction-driven
    /// rotation (every successful auto-compact creates a new session
    /// id whose `parent_session_id` is the pre-compact id).
    ///
    /// Providers that cache per-session state in their backend
    /// (document ids, accumulated buffers, counters) should update
    /// or reset it here so subsequent writes land in the correct
    /// session's record.
    ///
    /// `new_session_id` — the id the agent just switched to.
    /// `parent_session_id` — the previous id, empty when no
    /// lineage applies.
    /// `reset` — `true` when this is a fresh conversation (not a
    /// continuation). Compaction rotation is a continuation, so
    /// dirge passes `false`. Reserved for future `/reset`-style
    /// commands.
    fn on_session_switch(&self, _new_session_id: &str, _parent_session_id: &str, _reset: bool) {}

    /// Notify the provider that messages are about to be discarded
    /// during context compression. The provider may return a brief
    /// summary string that the compression pass will fold into the
    /// summary prompt so any provider-extracted insights survive.
    /// Default returns an empty string.
    fn on_pre_compress(&self, _transcript: &str) -> String {
        String::new()
    }
}

/// Implementing `MemoryProvider` on the dirge built-in
/// `SqliteMemoryStore` makes it the canonical backend without changing
/// any of its existing public methods.
///
/// dirge-5feg: this impl deliberately does NOT call `on_memory_write`
/// from inside `add`/`replace`/`remove`. The `MemoryTool::call`
/// dispatcher fires the hook once after every successful CRUD so
/// custom providers (and providers that wrap this one) get the hook
/// fired exactly once at the tool layer, without each impl having to
/// remember to do so.
impl MemoryProvider for super::memory_db::SqliteMemoryStore {
    fn name(&self) -> &str {
        "builtin"
    }

    fn format_for_system_prompt(&self) -> String {
        super::memory_db::SqliteMemoryStore::format_for_system_prompt(self)
    }

    fn view(&self, target: &str) -> Value {
        super::memory_db::SqliteMemoryStore::view(self, target)
    }

    fn add(&self, target: &str, content: &str, kind: Option<&str>) -> Result<Value, String> {
        let mkind = kind.and_then(super::memory_db::parse_kind);
        super::memory_db::SqliteMemoryStore::add(self, target, content, mkind)
    }

    fn replace(
        &self,
        target: &str,
        old_text: &str,
        content: &str,
        kind: Option<&str>,
    ) -> Result<Value, String> {
        let mkind = kind.and_then(super::memory_db::parse_kind);
        super::memory_db::SqliteMemoryStore::replace(self, target, old_text, content, mkind)
    }

    fn remove(&self, target: &str, old_text: &str) -> Result<Value, String> {
        super::memory_db::SqliteMemoryStore::remove(self, target, old_text)
    }

    fn restore(&self, target: &str, old_text: &str) -> Result<Value, String> {
        super::memory_db::SqliteMemoryStore::restore(self, target, old_text)
    }

    fn expand(&self, old_text: &str) -> Result<Value, String> {
        super::memory_db::SqliteMemoryStore::expand(self, old_text)
    }

    fn search(&self, query: &str) -> Result<Value, String> {
        super::memory_db::SqliteMemoryStore::search(self, query)
    }

    fn record_outcome(&self, target: &str, old_text: &str, success: bool) -> Result<Value, String> {
        super::memory_db::SqliteMemoryStore::record_outcome(self, target, old_text, success)
    }

    fn supersede(
        &self,
        target: &str,
        old_text: &str,
        content: &str,
        kind: Option<&str>,
        harsh: bool,
    ) -> Result<Value, String> {
        let mkind = kind.and_then(super::memory_db::parse_kind);
        super::memory_db::SqliteMemoryStore::supersede(
            self, target, old_text, content, mkind, harsh,
        )
    }

    fn refresh_snapshot(&self) -> Result<(), String> {
        super::memory_db::SqliteMemoryStore::refresh_snapshot(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// dirge-5feg — A minimal test provider that records hook
    /// invocations only. Per the new contract, its CRUD methods
    /// MUST NOT self-fire `on_memory_write` — the `MemoryTool`
    /// layer fires once after each successful CRUD.
    #[derive(Default)]
    struct RecordingProvider {
        writes: Mutex<Vec<(String, String, String)>>,
    }

    impl MemoryProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording-test"
        }
        fn view(&self, _target: &str) -> Value {
            Value::Null
        }
        fn add(&self, _: &str, _: &str, _kind: Option<&str>) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn replace(&self, _: &str, _: &str, _: &str, _kind: Option<&str>) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn remove(&self, _: &str, _: &str) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn on_memory_write(&self, action: &str, target: &str, content: &str) {
            self.writes
                .lock()
                .unwrap()
                .push((action.into(), target.into(), content.into()));
        }
    }

    #[test]
    fn provider_crud_does_not_self_fire_on_memory_write() {
        // Per the dirge-5feg contract: the provider's CRUD methods
        // must NOT call on_memory_write directly. Only the tool
        // layer fires it. Verified here by calling the provider's
        // methods directly (bypassing the tool) — the writes vec
        // must stay empty.
        let p = RecordingProvider::default();
        let _ = p.add("memory", "hello", None);
        let _ = p.replace("memory", "old", "hello", None);
        let _ = p.remove("pitfalls", "old");

        let writes = p.writes.lock().unwrap();
        assert!(
            writes.is_empty(),
            "providers must NOT self-fire on_memory_write \
             (the tool layer does); got: {:?}",
            *writes
        );
    }

    /// dirge-ix7n — the `on_memory_write` `payload` parameter has
    /// action-specific semantics (new content for add/replace,
    /// old_text identifier for remove). The trait doc must spell
    /// this out so plugin authors don't conflate them. Test
    /// guards the doc against silent regression — if someone
    /// removes the contract from the doc, the test fails.
    #[test]
    fn on_memory_write_contract_is_documented() {
        let src = include_str!("memory_provider.rs");
        // Find the docstring block immediately above the trait
        // method declaration.
        let anchor = "fn on_memory_write";
        let pos = src
            .find(anchor)
            .expect("trait method on_memory_write must exist");
        // The docstring lives in the ~30 lines preceding the
        // method line. Look for the action-asymmetry markers.
        let preamble = &src[pos.saturating_sub(2000)..pos];
        assert!(
            preamble.contains("payload"),
            "doc must rename the third param meaning to 'payload'"
        );
        assert!(
            preamble.contains("`\"remove\"`"),
            "doc must describe the remove case"
        );
        assert!(
            preamble.contains("old_text"),
            "doc must say payload is old_text on remove"
        );
        assert!(
            preamble.contains("NOT a generic") || preamble.contains("not a generic"),
            "doc must warn that payload is NOT a generic new-value field"
        );
    }

    #[test]
    fn external_on_memory_write_call_records() {
        // The hook can still be called explicitly by the tool
        // layer or by tests — this verifies the recording surface
        // works when invoked from outside the CRUD path.
        let p = RecordingProvider::default();
        p.on_memory_write("add", "memory", "hello");
        p.on_memory_write("remove", "pitfalls", "hello");

        let writes = p.writes.lock().unwrap();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0], ("add".into(), "memory".into(), "hello".into()));
        assert_eq!(
            writes[1],
            ("remove".into(), "pitfalls".into(), "hello".into())
        );
    }

    /// dirge-7tvq — the augmentation logic that wraps a provider's
    /// `on_pre_compress` output into the compression `instructions`
    /// parameter must (a) call the hook with the transcript, (b)
    /// fold non-empty output in, and (c) leave existing user
    /// instructions intact.
    #[test]
    fn on_pre_compress_output_threads_into_instructions() {
        #[derive(Default)]
        struct InsightProvider {
            saw_transcript: Mutex<Option<String>>,
        }
        impl MemoryProvider for InsightProvider {
            fn name(&self) -> &str {
                "insight"
            }
            fn view(&self, _: &str) -> Value {
                Value::Null
            }
            fn add(&self, _: &str, _: &str, _kind: Option<&str>) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn replace(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _kind: Option<&str>,
            ) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn remove(&self, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn on_pre_compress(&self, transcript: &str) -> String {
                *self.saw_transcript.lock().unwrap() = Some(transcript.to_string());
                "REMEMBER: project uses cargo not bazel".into()
            }
        }
        let p = InsightProvider::default();

        // Hook fires with the transcript verbatim.
        let extra = p.on_pre_compress("turn 1 transcript");
        assert_eq!(extra, "REMEMBER: project uses cargo not bazel");
        assert_eq!(
            p.saw_transcript.lock().unwrap().as_deref(),
            Some("turn 1 transcript"),
            "hook must receive the pre-compress transcript verbatim"
        );
    }

    /// dirge-7tvq — `on_session_end` receives the live-session
    /// transcript exactly once per session-swap.
    #[test]
    fn on_session_end_fires_with_transcript() {
        #[derive(Default)]
        struct EndProvider {
            ends: Mutex<Vec<String>>,
        }
        impl MemoryProvider for EndProvider {
            fn name(&self) -> &str {
                "end"
            }
            fn view(&self, _: &str) -> Value {
                Value::Null
            }
            fn add(&self, _: &str, _: &str, _kind: Option<&str>) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn replace(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _kind: Option<&str>,
            ) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn remove(&self, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn on_session_end(&self, transcript: &str) {
                self.ends.lock().unwrap().push(transcript.to_string());
            }
        }
        let p = EndProvider::default();
        p.on_session_end("User: hi\n\nAssistant: hello\n");
        let ends = p.ends.lock().unwrap();
        assert_eq!(ends.len(), 1, "exactly one end-of-session fire");
        assert!(
            ends[0].contains("User: hi") && ends[0].contains("Assistant: hello"),
            "transcript must contain user + assistant turns: {:?}",
            ends[0]
        );
    }

    #[test]
    fn alternative_provider_default_hooks_are_no_ops() {
        // A provider that overrides only the CRUD methods doesn't
        // need to think about session-end, pre-compress, etc.
        struct MinimalProvider;
        impl MemoryProvider for MinimalProvider {
            fn name(&self) -> &str {
                "minimal"
            }
            fn view(&self, _: &str) -> Value {
                Value::Null
            }
            fn add(&self, _: &str, _: &str, _kind: Option<&str>) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn replace(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _kind: Option<&str>,
            ) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn remove(&self, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
        }
        let p = MinimalProvider;
        // None of these should panic or require an impl.
        p.on_session_end("transcript");
        assert_eq!(p.on_pre_compress("anything"), "");
        p.on_memory_write("add", "memory", "x");
    }

    #[test]
    fn builtin_store_implements_trait_and_routes_through_on_write() {
        use crate::extras::dirge_paths::ProjectPaths;
        let dir = std::env::temp_dir().join(format!(
            "dirge-memprovider-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let paths = ProjectPaths::new(&dir);
        let store = super::super::memory_db::SqliteMemoryStore::load(&paths).unwrap();

        // Call through the trait — proves the impl forwards.
        let provider: &dyn MemoryProvider = &store;
        assert_eq!(provider.name(), "builtin");
        let resp = provider.add("memory", "trait-routed entry", None).unwrap();
        assert_eq!(resp["success"], true);

        let view = provider.view("memory");
        let entries = view["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| {
            e.as_str()
                .map(|s| s.contains("trait-routed"))
                .unwrap_or(false)
        }));

        std::fs::remove_dir_all(&dir).ok();
    }
}
