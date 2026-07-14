use std::path::PathBuf;

use crate::session::Session;

fn session_dir() -> PathBuf {
    dirs_path().join("sessions")
}

/// Per-session asset directory: `<sessions>/assets/<id>/`. Holds the
/// PNG bytes referenced by `SessionMessage.images` (`<asset_id>.png`
/// each). Removed wholesale when the session is deleted. The session
/// id is re-validated so a malformed id can't escape the assets root.
pub(crate) fn assets_dir_for(id: &str) -> PathBuf {
    // Enforce validation: a malformed id (traversal chars, `..`) is
    // remapped to a fixed in-root placeholder so it can never escape the
    // assets root via `Path::join`. Legit ids are server-generated UUIDs
    // validated at creation, so this only ever triggers on a tampered id.
    let base = session_dir().join("assets");
    if validate_session_id(id).is_ok() {
        base.join(id)
    } else {
        base.join("_invalid")
    }
}

#[cfg(not(test))]
fn home_fallback() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub(crate) fn dirs_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("DIRGE_DATA_DIR") {
        return PathBuf::from(dir);
    }
    // dirge-sn1k: tests must NEVER write to the user's real data dir.
    // Several tests exercise the runtime `new_session` handler, which
    // calls `save_session` — and they build fixtures with model "m" and
    // "stale"/"outgoing" messages. Without isolation those persist into
    // ~/.../dirge/sessions and pollute the user's recent-sessions list on
    // every `cargo test`. Route all test writes to a per-process temp dir
    // (DIRGE_DATA_DIR still wins, so tests can pick their own location).
    #[cfg(test)]
    {
        std::env::temp_dir().join(format!("dirge-test-data-{}", std::process::id()))
    }
    #[cfg(not(test))]
    {
        let base = dirs::data_dir().unwrap_or_else(home_fallback);
        base.join("dirge")
    }
}

/// Path to the GLOBAL, cross-project memory database. Unlike per-project
/// memory (under each repo's `.dirge/sessions/state.db`), this is a single
/// db in the user data dir so durable user preferences follow the user
/// across every project.
pub(crate) fn global_memory_db_path() -> PathBuf {
    dirs_path().join("global-memory.db")
}

pub(crate) fn config_path() -> PathBuf {
    config_path_from(
        std::env::var_os("DIRGE_CONFIG_DIR"),
        std::env::var_os("XDG_CONFIG_HOME"),
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
    )
}

/// Resolve the dirge config directory, precedence:
///   1. `DIRGE_CONFIG_DIR` — explicit dirge override.
///   2. `$XDG_CONFIG_HOME/dirge` — the XDG base-dir spec (only when set to an
///      absolute path, as the spec requires; relative values are ignored).
///   3. `~/.config/dirge` — the XDG default.
///
/// Pure (env values passed in) so it's testable without touching process env.
fn config_path_from(
    dirge_config_dir: Option<std::ffi::OsString>,
    xdg_config_home: Option<std::ffi::OsString>,
    home: PathBuf,
) -> PathBuf {
    if let Some(dir) = dirge_config_dir.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = xdg_config_home.filter(|d| !d.is_empty()) {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return xdg.join("dirge");
        }
    }
    home.join(".config").join("dirge")
}

/// Validate that a session id is safe to interpolate into a path.
/// Session ids are normally UUIDs (hex + hyphens), but they round-trip
/// through JSON on disk so a tampered-with file could carry an id like
/// `../../etc/passwd`. Reject anything that isn't strictly
/// `[A-Za-z0-9._-]+` so a malicious id can't escape the session dir.
pub(crate) fn validate_session_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() {
        anyhow::bail!("session id is empty");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        anyhow::bail!("session id contains disallowed characters: {:?}", id);
    }
    // Belt-and-braces: `..` or leading `.` would still resolve relatively
    // via `Path::join` even after the char check (`.` is allowed for
    // legitimate ids like `2024.session`).
    if id == "." || id == ".." || id.contains("/") || id.contains("\\") {
        anyhow::bail!("session id resolves outside the session dir: {:?}", id);
    }
    Ok(())
}

pub fn save_session(session: &mut Session) -> anyhow::Result<()> {
    validate_session_id(&session.id)?;
    // SESS-15: refuse to save a session that was loaded from a
    // file with `schema_version > SCHEMA_VERSION`. Newer-version
    // fields silently zeroed via `#[serde(default)]` at load;
    // writing the truncated form would permanently lose data the
    // newer dirge cared about. Better to surface an explicit
    // error so the user upgrades dirge instead of silently
    // corrupting their session.
    if let Some(file_version) = session.loaded_from_newer_version {
        anyhow::bail!(
            "refusing to save session {}: it was loaded from a newer schema (file version {}, this dirge supports {}). Upgrade dirge, or copy the session to a new id to write a fresh file.",
            session.id,
            file_version,
            crate::session::SCHEMA_VERSION,
        );
    }
    let dir = session_dir();
    std::fs::create_dir_all(&dir)?;
    // SESS-3: restrict session directory to owner-only (0700).
    // Session files contain user prompts, file contents, and
    // command outputs — other users on multi-user hosts should
    // not be able to list session IDs.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    // Snapshot the live panel globals into the session so a resume restores
    // the TODOS / MODIFIED panels even after a destructive compaction has
    // drained the originating tool calls out of `messages`. `save_session`
    // is the single persistence chokepoint and only ever runs for the main
    // interactive session (subagents don't call it), so the process-global
    // statics map to this session. See `session::rehydrate`.
    session.todo_list = crate::agent::tools::todo::snapshot();
    session.modified_files = crate::agent::tools::modified::recent(256)
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    let path = dir.join(format!("{}.json", session.id));
    let json = serde_json::to_string_pretty(session)?;

    // Batch2-3 (audit fix): concurrent-writer detection. If another
    // dirge instance saved to this file since we loaded it (i.e. the
    // on-disk mtime is newer than `session.loaded_mtime`), writing
    // verbatim would clobber the other instance's work. Divert to a
    // `<id>.conflict-<unix_ts>.json` sibling so neither side loses
    // data, and surface a clear error so the UI's "save failed"
    // warning explains the situation.
    if let Some(loaded_mtime) = session.loaded_mtime
        && let Ok(meta) = std::fs::metadata(&path)
        && let Ok(disk_mtime) = meta.modified()
        && disk_mtime > loaded_mtime
    {
        let ts = crate::time_util::now_unix_secs();
        let conflict_path = dir.join(format!("{}.conflict-{}.json", session.id, ts));
        crate::fs_atomic::atomic_write_sync(&conflict_path, json.as_bytes())?;
        anyhow::bail!(
            "session {} was modified by another dirge instance; your changes saved to {} so neither copy is lost. Reload the session to see the other instance's state.",
            session.id,
            conflict_path.display()
        );
    }

    // Atomic write — write to a sibling `.tmp.<nonce>` file,
    // fsync, then rename over the target. A crash mid-write leaves
    // the temp behind but never a truncated `.json`. POSIX
    // rename(2) is atomic on the same filesystem; the helper picks
    // a temp in the same parent dir to preserve that invariant.
    //
    // Extracted into `crate::fs_atomic` so this path + the
    // file-mutating tools (`write`/`edit`/`apply_patch`) share one
    // implementation. Previously the tools called
    // `tokio::fs::write` directly which truncates in place — a
    // corruption vector on crash.
    crate::fs_atomic::atomic_write_sync(&path, json.as_bytes())?;

    // dirge-yrql: refresh `loaded_mtime` to OUR write's mtime. Without this,
    // a resumed session (loaded_mtime = Some) would mistake its own previous
    // write for a concurrent instance on the NEXT save — the first save
    // advances the file mtime past the stale `loaded_mtime`, so every later
    // save would conflict-divert to a `.conflict-*.json` and the real file
    // would stop updating. Re-stat after the rename so the recorded mtime is
    // the file's actual on-disk value; a truly concurrent write after this
    // point is still caught (its mtime will exceed what we just recorded).
    if let Ok(meta) = std::fs::metadata(&path) {
        session.loaded_mtime = meta.modified().ok();
    }
    Ok(())
}

pub fn load_session(id: &str) -> anyhow::Result<Session> {
    validate_session_id(id)?;
    let dir = session_dir();
    let path = dir.join(format!("{}.json", id));
    // Batch2-3 (audit fix): record file mtime BEFORE reading so the
    // conflict check in save_session compares against the version
    // we actually loaded, not whatever has happened to the file
    // since. There's still a tiny window between metadata() and
    // read_to_string() — but the rename-based atomic_write makes
    // it impossible to see a torn read; if a concurrent writer
    // landed in that window we'll just detect THEIR version's
    // mtime, and our next save_session will conflict-divert.
    let loaded_mtime = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());
    let json = std::fs::read_to_string(&path)?;

    // F8: schema-version handling. Pre-F8 session files have no
    // `schema_version` field; serde defaults it to 0. New
    // sessions are at `SCHEMA_VERSION`. Anything in between gets
    // migrated. A file with schema_version > SCHEMA_VERSION
    // (forward-incompatible) loads with a warning — most fields
    // still deserialize via `#[serde(default)]`, just the new
    // ones get default values.
    let mut session: Session = serde_json::from_str(&json).map_err(|e| {
        // Add file-path context to corrupted-file errors so the
        // user knows which session is broken and can recover by
        // restoring from a backup or deleting.
        anyhow::anyhow!("failed to parse {}: {e}", path.display())
    })?;
    session.loaded_mtime = loaded_mtime;

    if session.schema_version < crate::session::SCHEMA_VERSION {
        migrate_session(&mut session);
        session.schema_version = crate::session::SCHEMA_VERSION;
    } else if session.schema_version > crate::session::SCHEMA_VERSION {
        tracing::warn!(
            target: "dirge::session",
            path = %path.display(),
            file_version = session.schema_version,
            our_version = crate::session::SCHEMA_VERSION,
            "session file is from a newer dirge version; unknown fields will default. Upgrade dirge to read it fully."
        );
        // SESS-15: remember that the file was newer so save_session
        // can refuse to overwrite. Otherwise a downgrade-then-save
        // permanently loses the newer-version fields that
        // #[serde(default)] silently zeroed at load.
        session.loaded_from_newer_version = Some(session.schema_version.into());
    }
    Ok(session)
}

/// Bring a session loaded from an older schema version up to the
/// current `SCHEMA_VERSION`. Idempotent. Each migration step
/// handles one version bump; chain them as we add versions.
///
/// Current state: SCHEMA_VERSION = 1, which is "schema-versioned"
/// vs. pre-F8 (treated as 0). No data shape changes between
/// version 0 and 1 — the field additions for branch_summaries,
/// tool_calls, current_prompt_name etc. all used
/// `#[serde(default)]` so they already migrate transparently.
/// This function exists so future schema bumps have a hook.
fn migrate_session(session: &mut Session) {
    // v0 → v1: no-op (back-compat handled entirely via `#[serde(default)]`).
    // v1 → v2: recompute `estimated_tokens` for every message + the
    // session's `total_estimated_tokens` because pre-9a044ce sessions
    // counted only assistant TEXT — tool args and tool results were
    // ignored. Without this migration, a resumed long-running session
    // shows a context usage 5–10× under reality and could silently
    // exceed the model's actual context window before any compress
    // fires.
    if session.schema_version < 2 {
        session.recompute_all_estimates();
    }
}

pub fn delete_session(id: &str) -> anyhow::Result<()> {
    validate_session_id(id)?;
    let dir = session_dir();

    // dirge-vpma.13: a session and its compaction-fold rotations share one
    // effective_origin. Deleting only <id>.json leaves the older rotations
    // on disk, and find_recent_sessions resurfaces the pre-fold rotation as
    // fully resumable — the "deleted" conversation comes back. Delete every
    // member of the origin chain, not just the tip the caller named. Uses a
    // cheap partial parse (id + origin_id only), like load_session_tip.
    #[derive(serde::Deserialize)]
    struct OriginMeta {
        id: String,
        #[serde(default)]
        origin_id: Option<String>,
    }
    fn effective_origin(m: &OriginMeta) -> &str {
        m.origin_id.as_deref().unwrap_or(&m.id)
    }

    // Resolve the target's origin. If it can't be resolved (file already
    // gone or unparseable), fall back to deleting just the named id so the
    // behaviour never regresses below the old single-file delete.
    let target_origin = std::fs::read_to_string(dir.join(format!("{}.json", id)))
        .ok()
        .and_then(|json| serde_json::from_str::<OriginMeta>(&json).ok())
        .map(|m| effective_origin(&m).to_string());

    let members: Vec<String> = match target_origin {
        Some(origin) => {
            let mut ids = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "json")
                        && let Ok(json) = std::fs::read_to_string(&path)
                        && let Ok(meta) = serde_json::from_str::<OriginMeta>(&json)
                        && effective_origin(&meta) == origin
                    {
                        ids.push(meta.id);
                    }
                }
            }
            if ids.is_empty() {
                vec![id.to_string()]
            } else {
                ids
            }
        }
        None => vec![id.to_string()],
    };

    for member in members {
        // Member ids come from on-disk files; re-validate before joining.
        if validate_session_id(&member).is_err() {
            continue;
        }
        let path = dir.join(format!("{}.json", member));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        // Drop the member's image assets (whole dir). Best-effort: a
        // missing assets dir (pre-image sessions) is not an error.
        let assets = assets_dir_for(&member);
        if assets.exists() {
            std::fs::remove_dir_all(assets)?;
        }
    }
    Ok(())
}

/// Collapse fold chains to one entry per conversation: keep only the
/// first session seen for each [`Session::effective_origin`]. Callers
/// pass a newest-first list, so the survivor is the chain tip and the
/// stale older rotations drop out — the session list shows a folded
/// conversation once, not once per rotation. Pure for testability.
fn dedup_by_origin(sessions: Vec<Session>) -> Vec<Session> {
    let mut seen = std::collections::HashSet::new();
    sessions
        .into_iter()
        .filter(|s| seen.insert(s.effective_origin().to_string()))
        .collect()
}

/// Resume helper: resolve `id` to the TIP of its fold chain. A compaction
/// fold rotates the session id and leaves the older file behind unchanged
/// (pre-fold state), so resuming *any* member of a chain must hop to the
/// newest session that shares the same [`Session::effective_origin`] —
/// otherwise resume silently loads a stale snapshot. Falls back to the
/// directly-loaded session when nothing newer shares its origin (the
/// common case: the user already named the tip, or the session never
/// folded). Filtering by origin makes the directory scan robust to
/// unrelated sessions.
pub fn load_session_tip(id: &str) -> anyhow::Result<Session> {
    let requested = load_session(id)?;
    let origin = requested.effective_origin().to_string();
    let dir = session_dir();
    if !dir.exists() {
        return Ok(requested);
    }

    // Find the chain tip with a CHEAP partial parse. A fold leaves the older
    // (often large) session file behind, so these accumulate; fully
    // deserializing every one — every message, tool result, and the
    // `message_store` map — on each resume was the dominant cost of
    // `--session` startup. `TipMeta` pulls only the three fields the scan
    // compares; serde skips everything else via `IgnoredAny`, never
    // allocating the message structures. We then fully load just the winner.
    #[derive(serde::Deserialize)]
    struct TipMeta {
        id: String,
        #[serde(default)]
        origin_id: Option<String>,
        #[serde(default)]
        updated_at: String,
    }

    let mut tip_id = requested.id.to_string();
    let mut tip_updated = requested.updated_at.to_string();
    for entry in std::fs::read_dir(&dir)? {
        // Skip a bad directory entry rather than aborting resume: a
        // concurrent fold/cleanup can delete a sibling file mid-scan, and
        // the seed session already loaded fine — a plain load would have
        // succeeded, so the tip scan must degrade to it, not error out.
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(meta) = serde_json::from_str::<TipMeta>(&json)
        {
            let meta_origin = meta.origin_id.as_deref().unwrap_or(&meta.id);
            if meta_origin == origin && meta.updated_at > tip_updated {
                tip_updated = meta.updated_at;
                tip_id = meta.id;
            }
        }
    }

    if tip_id.as_str() == requested.id.as_str() {
        // Seed is already the tip — no second load.
        Ok(requested)
    } else {
        // Fully deserialize only the winner. If it vanished between the scan
        // and this load (concurrent fold/cleanup), fall back to the seed we
        // already have rather than failing the resume.
        Ok(load_session(&tip_id).unwrap_or(requested))
    }
}

pub fn find_sessions_by_prefix(prefix: &str) -> anyhow::Result<Vec<Session>> {
    // SESS-5: `stem.starts_with("")` matches every session file, so
    // `/sessions ` or `/sessions delete ` (trailing space) would
    // enumerate or operate on ALL sessions instead of a prefix
    // match. Reject empty prefix so callers must supply at least
    // one character.
    if prefix.is_empty() {
        anyhow::bail!("session prefix must not be empty");
    }
    let dir = session_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions: Vec<Session> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && stem.starts_with(prefix)
            && let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<Session>(&json)
        {
            sessions.push(session);
        }
    }
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    // Collapse fold chains so a prefix that spans a rotated conversation
    // returns the single tip, not every rotation.
    Ok(dedup_by_origin(sessions))
}

pub fn find_recent_sessions(limit: usize) -> anyhow::Result<Vec<Session>> {
    let dir = session_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // Audit L10: previously read + parsed every `*.json` then sorted
    // by `updated_at` then truncated. For a user with 5 000 stored
    // sessions this is 5 000 file reads + parses on every `/sessions`
    // invocation. Sort by filesystem mtime first (cheap; uses the
    // metadata already read by `read_dir`), then parse only the top
    // `limit`. mtime corresponds closely to `updated_at` since both
    // are bumped on every `save_session` write.
    let mut entries: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        entries.push((path, mtime));
    }
    // Newest first.
    entries.sort_by_key(|e| std::cmp::Reverse(e.1));
    entries.truncate(limit);

    let mut sessions: Vec<Session> = Vec::with_capacity(entries.len());
    for (path, _) in entries {
        if let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<Session>(&json)
        {
            sessions.push(session);
        }
    }
    // Refine ordering by the in-file updated_at — mtime is a good
    // proxy but `updated_at` is canonical. Cheap on the already-
    // truncated list.
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    // Show one row per conversation: a just-folded chain can have both
    // its tip and a recent older rotation inside the window; keep the
    // tip. Stale rotations from older folds already sink below the
    // window by mtime. (A chain occupying the window can yield slightly
    // fewer than `limit` rows — acceptable for a recents list.)
    Ok(dedup_by_origin(sessions))
}

/// Cross-session Up-arrow history: the `max_sessions` most-recent OTHER
/// sessions in the same project (same `working_dir`, different
/// conversation origin than `current`). Returned OLDEST-first so the
/// caller can append their prompts ahead of the current session's own,
/// keeping the newest command at the tail of history (where Up-arrow
/// recall starts).
///
/// Candidate files are ordered by filesystem mtime (free from `read_dir`
/// metadata) and visited newest-first, so only enough files to find the
/// `max_sessions` most-recent same-project conversations are read — a
/// large session store isn't loaded wholesale on startup. mtime tracks
/// `updated_at` (both bumped on every `save_session` write), the same
/// recency proxy `find_recent_sessions` relies on. A cheap partial parse
/// (`ProjMeta`) filters by `working_dir` before the few winners are fully
/// deserialized. Fold chains are collapsed by origin — the first file
/// seen for an origin is its newest-mtime tip — so `max_sessions` counts
/// distinct prior conversations, not rotation files, and a folded prior
/// can't seed overlapping prompts.
pub fn recent_project_sessions(current: &Session, max_sessions: usize) -> Vec<Session> {
    if max_sessions == 0 {
        return Vec::new();
    }
    let dir = session_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let current_origin = current.effective_origin();
    let current_wd = current.working_dir.as_str();

    #[derive(serde::Deserialize)]
    struct ProjMeta {
        id: String,
        #[serde(default)]
        origin_id: Option<String>,
        #[serde(default)]
        working_dir: Option<String>,
    }

    // Order by mtime newest-first using only the directory metadata — no
    // file content read yet.
    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        files.push((path, mtime));
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.1));

    // Walk newest-first, partial-parsing only until the `max_sessions`
    // most-recent same-project conversations are found. Pre-seeding `seen`
    // with the current origin skips the current session's own fold chain;
    // the first file seen for any other origin is its tip, so the set also
    // collapses rotations. A vanished or unparseable sibling is skipped
    // rather than aborting (concurrent fold/cleanup can rewrite a file
    // mid-scan).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(current_origin.to_string());
    let mut picked: Vec<std::path::PathBuf> = Vec::new();
    for (path, _) in files {
        if picked.len() == max_sessions {
            break;
        }
        let Ok(json) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<ProjMeta>(&json) else {
            continue;
        };
        if meta.working_dir.as_deref() != Some(current_wd) {
            continue;
        }
        let origin = meta.origin_id.unwrap_or(meta.id);
        if !seen.insert(origin) {
            continue;
        }
        picked.push(path);
    }
    // Oldest-first: appending then yields oldest-session prompts first,
    // so the newest prior session sits just behind the current session.
    picked.reverse();

    let mut out = Vec::with_capacity(picked.len());
    for path in picked {
        if let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<Session>(&json)
        {
            out.push(session);
        }
    }
    out
}

pub fn agents_path() -> PathBuf {
    config_path().join("agent").join("AGENTS.md")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn config_path_prefers_dirge_config_dir() {
        let p = config_path_from(
            Some(OsString::from("/explicit/dirge")),
            Some(OsString::from("/xdg")),
            PathBuf::from("/home/u"),
        );
        assert_eq!(p, PathBuf::from("/explicit/dirge"));
    }

    #[test]
    fn config_path_honors_xdg_config_home() {
        let p = config_path_from(
            None,
            Some(OsString::from("/xdg/cfg")),
            PathBuf::from("/home/u"),
        );
        assert_eq!(p, PathBuf::from("/xdg/cfg/dirge"));
    }

    #[test]
    fn config_path_ignores_relative_xdg_and_falls_back_to_home() {
        // The XDG spec says XDG_CONFIG_HOME must be absolute; a relative value
        // is treated as unset.
        let p = config_path_from(
            None,
            Some(OsString::from("relative/cfg")),
            PathBuf::from("/home/u"),
        );
        assert_eq!(p, PathBuf::from("/home/u/.config/dirge"));
    }

    #[test]
    fn config_path_defaults_to_home_config_dirge() {
        let p = config_path_from(None, None, PathBuf::from("/home/u"));
        assert_eq!(p, PathBuf::from("/home/u/.config/dirge"));
    }

    #[test]
    fn config_path_treats_empty_overrides_as_unset() {
        let p = config_path_from(
            Some(OsString::from("")),
            Some(OsString::from("")),
            PathBuf::from("/home/u"),
        );
        assert_eq!(p, PathBuf::from("/home/u/.config/dirge"));
    }

    /// dirge-sn1k: under test, the data dir must route to a per-process
    /// temp location (never the user's real ~/.../dirge), so persistence
    /// tests can't leak fixtures into the recent-sessions list. (An
    /// explicit DIRGE_DATA_DIR override still wins.)
    #[test]
    fn test_data_dir_is_isolated_to_temp() {
        if std::env::var_os("DIRGE_DATA_DIR").is_some() {
            return; // explicit override — isolation is the caller's choice
        }
        let p = dirs_path();
        assert!(
            p.starts_with(std::env::temp_dir()),
            "test data dir must be under temp, got {p:?}"
        );
        assert!(p.to_string_lossy().contains("dirge-test-data"), "got {p:?}");
        // And the real data dir must NOT be the session location in tests.
        assert!(
            !session_dir()
                .to_string_lossy()
                .contains("Application Support/dirge")
                && !session_dir()
                    .to_string_lossy()
                    .ends_with(".local/share/dirge/sessions"),
            "session_dir leaked to the real data dir: {:?}",
            session_dir()
        );
    }

    /// Resuming any member of a fold chain resolves to the live tip —
    /// the newest session sharing the origin — not the stale older file
    /// the fold left behind. A unique origin keeps the shared test data
    /// dir's scan from colliding with other tests' sessions.
    #[test]
    fn load_session_tip_resolves_to_newest_in_chain() {
        use crate::session::{MessageRole, Session};
        let origin = format!("origin-{}", uuid::Uuid::new_v4().simple());

        // Original session: its own origin (origin_id None), older.
        let mut old = Session::new("p", "m", 128_000);
        old.id = compact_str::CompactString::new(origin.clone());
        old.add_message(MessageRole::User, "the original ask");
        old.updated_at = compact_str::CompactString::new("2026-01-01T00:00:00+00:00");
        save_session(&mut old).unwrap();

        // Rotated tip: shares the origin, newer.
        let mut tip = Session::new("p", "m", 128_000);
        tip.id =
            compact_str::CompactString::new(format!("compacted-{}", uuid::Uuid::new_v4().simple()));
        tip.origin_id = Some(compact_str::CompactString::new(origin.clone()));
        tip.add_message(MessageRole::User, "newer state");
        tip.updated_at = compact_str::CompactString::new("2026-06-01T00:00:00+00:00");
        save_session(&mut tip).unwrap();

        // Resuming the ORIGINAL id hops forward to the tip.
        let resolved = load_session_tip(&origin).unwrap();
        assert_eq!(
            resolved.id, tip.id,
            "resuming the original id must land on the chain tip"
        );

        // Resuming the tip id stays on the tip.
        let resolved2 = load_session_tip(tip.id.as_str()).unwrap();
        assert_eq!(resolved2.id, tip.id);

        // An UNRELATED session that is newer than the tip but has a different
        // origin must NOT be selected — the cheap partial-parse scan still
        // filters by origin. Guards the load_session_tip refactor.
        let mut unrelated = Session::new("p", "m", 128_000);
        unrelated.id =
            compact_str::CompactString::new(format!("other-{}", uuid::Uuid::new_v4().simple()));
        unrelated.add_message(MessageRole::User, "different conversation");
        unrelated.updated_at = compact_str::CompactString::new("2030-01-01T00:00:00+00:00");
        save_session(&mut unrelated).unwrap();

        let resolved3 = load_session_tip(&origin).unwrap();
        assert_eq!(
            resolved3.id, tip.id,
            "a newer session from a different origin must not be chosen as the tip"
        );
    }

    /// A folded conversation collapses to its tip in a listing: the
    /// newest member of each origin survives, standalone sessions are
    /// untouched, and ordering is preserved.
    #[test]
    fn dedup_by_origin_keeps_tip_per_conversation() {
        use crate::session::Session;
        let mk = |id: &str, origin: Option<&str>, updated: &str| {
            let mut s = Session::new("p", "m", 0);
            s.id = compact_str::CompactString::new(id);
            s.origin_id = origin.map(compact_str::CompactString::new);
            s.updated_at = compact_str::CompactString::new(updated);
            s
        };
        // Newest-first input, as callers pass it. Chain "conv": tip then
        // its stale older rotation. Plus a standalone session.
        let input = vec![
            mk("compacted-tip", Some("conv"), "2026-06-01T00:00:00+00:00"),
            mk("standalone", None, "2026-05-01T00:00:00+00:00"),
            mk("conv", None, "2026-01-01T00:00:00+00:00"), // origin == its own id
        ];
        let out = dedup_by_origin(input);
        let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["compacted-tip", "standalone"],
            "the chain collapses to its tip; standalone stays"
        );
    }

    /// Cross-session Up-arrow history: `recent_project_sessions` returns
    /// the `max_sessions` most-recent OTHER sessions in the same project
    /// (same `working_dir`, different conversation origin), oldest-first,
    /// and excludes other-project sessions plus the current conversation's
    /// own fold-chain members.
    #[test]
    fn recent_project_sessions_filters_and_orders() {
        use crate::session::{MessageRole, Session, SessionMessage};
        let dir = session_dir();
        let _ = std::fs::create_dir_all(&dir);
        // Unique project per run so other tests' session files can't leak in.
        let proj = format!(
            "/tmp/dirge-hist-proj-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let other = "/tmp/dirge-hist-other-project";

        let seed = |id: &str, origin: Option<&str>, updated: &str, wd: &str, msgs: &[&str]| {
            let mut s = Session::new("p", "m", 0);
            s.id = compact_str::CompactString::new(id);
            s.origin_id = origin.map(compact_str::CompactString::new);
            s.updated_at = compact_str::CompactString::new(updated);
            s.working_dir = compact_str::CompactString::new(wd);
            s.messages = msgs
                .iter()
                .map(|t| SessionMessage {
                    role: MessageRole::User,
                    content: compact_str::CompactString::new(*t),
                    estimated_tokens: 0,
                    id: compact_str::CompactString::new("m"),
                    timestamp: 0,
                    tool_calls: Vec::new(),
                    images: Vec::new(),
                })
                .collect();
            let path = dir.join(format!("{id}.json"));
            std::fs::write(&path, serde_json::to_string(&s).unwrap()).unwrap();
            // recent_project_sessions ranks by file mtime; sleep so each
            // seed's write lands a strictly-later mtime regardless of the
            // filesystem's timestamp granularity.
            std::thread::sleep(std::time::Duration::from_millis(10));
            id.to_string()
        };

        let t1 = "2026-01-01T00:00:00+00:00";
        let t2 = "2026-02-01T00:00:00+00:00";
        let t3 = "2026-03-01T00:00:00+00:00";
        let t4 = "2026-04-01T00:00:00+00:00";
        let t5 = "2026-05-01T00:00:00+00:00";

        // A < B < C are real same-project priors of increasing recency.
        let a = seed("hist-a", None, t1, &proj, &["a1"]);
        let b = seed("hist-b", None, t2, &proj, &["b1", "b2"]);
        let c = seed("hist-c", None, t3, &proj, &["c1"]);
        // D: newest of all, but a DIFFERENT project → must be dropped.
        let _d = seed("hist-d", None, t4, other, &["d1"]);
        // F: same project, newest, but shares the current conversation's
        // origin (a stale fold rotation) → must be dropped.
        let _f = seed("hist-f", Some("hist-current"), t5, &proj, &["f1"]);

        // Current session lives only in memory (brand-new, not yet saved).
        let mut current = Session::new("p", "m", 0);
        current.id = compact_str::CompactString::new("hist-current");
        current.working_dir = compact_str::CompactString::new(&proj);

        // Cap at 2: the two most-recent priors are C and B, returned
        // oldest-first → [B, C]. A is too old; D (other project) and F
        // (current's own origin) are excluded.
        let out = recent_project_sessions(&current, 2);
        let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["hist-b", "hist-c"]);

        // Messages survive the round-trip and stay in entry order.
        assert_eq!(
            out[0]
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            vec!["b1", "b2"],
        );

        // Uncapped → all three same-project priors, oldest-first.
        let out_all = recent_project_sessions(&current, 10);
        let ids_all: Vec<&str> = out_all.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids_all, vec!["hist-a", "hist-b", "hist-c"]);

        // max_sessions 0 disables mining entirely.
        assert!(recent_project_sessions(&current, 0).is_empty());

        for id in [a.as_str(), b.as_str(), c.as_str(), "hist-d", "hist-f"] {
            let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        }
    }

    /// A prior conversation that folded leaves several rotation files
    /// sharing one origin. `recent_project_sessions` must collapse them to
    /// the chain tip so `max_sessions` counts conversations, not rotations,
    /// and overlapping prompts aren't seeded twice.
    #[test]
    fn recent_project_sessions_collapses_fold_chains() {
        use crate::session::{MessageRole, Session, SessionMessage};
        let dir = session_dir();
        let _ = std::fs::create_dir_all(&dir);
        let proj = format!(
            "/tmp/dirge-hist-fold-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let seed = |id: &str, origin: Option<&str>, updated: &str, wd: &str, msgs: &[&str]| {
            let mut s = Session::new("p", "m", 0);
            s.id = compact_str::CompactString::new(id);
            s.origin_id = origin.map(compact_str::CompactString::new);
            s.updated_at = compact_str::CompactString::new(updated);
            s.working_dir = compact_str::CompactString::new(wd);
            s.messages = msgs
                .iter()
                .map(|t| SessionMessage {
                    role: MessageRole::User,
                    content: compact_str::CompactString::new(*t),
                    estimated_tokens: 0,
                    id: compact_str::CompactString::new("m"),
                    timestamp: 0,
                    tool_calls: Vec::new(),
                    images: Vec::new(),
                })
                .collect();
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_string(&s).unwrap(),
            )
            .unwrap();
            // recent_project_sessions ranks by file mtime; sleep so each
            // rotation's write lands a strictly-later mtime (the newest is
            // the chain tip) regardless of filesystem timestamp granularity.
            std::thread::sleep(std::time::Duration::from_millis(10));
            id.to_string()
        };

        let t1 = "2026-01-01T00:00:00+00:00";
        let t2 = "2026-02-01T00:00:00+00:00";
        let t3 = "2026-03-01T00:00:00+00:00";

        // One prior conversation, three rotations sharing origin "fold-r1".
        // r1 (the origin file) is oldest; r3 is the tip.
        let r1 = seed("fold-r1", None, t1, &proj, &["old"]);
        let r2 = seed("fold-r2", Some("fold-r1"), t2, &proj, &["mid"]);
        let r3 = seed("fold-r3", Some("fold-r1"), t3, &proj, &["tip"]);

        let mut current = Session::new("p", "m", 0);
        current.id = compact_str::CompactString::new("fold-current");
        current.working_dir = compact_str::CompactString::new(&proj);

        // Even uncapped, the three rotations collapse to a single session —
        // the tip (newest updated_at) — not three separate slots.
        let out = recent_project_sessions(&current, 10);
        let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["fold-r3"], "fold chain collapses to its tip");

        for id in [r1.as_str(), r2.as_str(), r3.as_str()] {
            let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        }
    }

    #[test]
    fn validate_session_id_accepts_uuids() {
        assert!(validate_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890").is_ok());
        assert!(validate_session_id("plain-id").is_ok());
        assert!(validate_session_id("2024.session").is_ok());
        assert!(validate_session_id("session_42").is_ok());
    }

    /// Review #2: v1 → v2 migration recomputes
    /// `estimated_tokens` because pre-9a044ce sessions counted
    /// only assistant TEXT. A v1 session JSON with under-counted
    /// values must come up with the new (correct) higher count.
    #[test]
    fn v1_to_v2_recomputes_under_counted_estimates() {
        use crate::session::{MessageRole, Session, SessionMessage, ToolCallEntry, ToolCallState};
        // Build a v1-shape session manually with a tool call whose
        // result is 8000 chars but estimated_tokens reflects only
        // the assistant text (5 chars / 4 = 1).
        let mut s = Session::new("p", "m", 128_000);
        // Forcibly create a message that mimics the pre-9a044ce
        // accounting (skip add_message_with_tool_calls' new logic).
        let tc = ToolCallEntry {
            id: "t1".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"command": "..."}),
            state: ToolCallState::Completed {
                result: "x".repeat(8000),
            },
        };
        let msg = SessionMessage {
            role: MessageRole::Assistant,
            content: compact_str::CompactString::new("hello"),
            estimated_tokens: 1, // ← under-counted on purpose
            id: compact_str::CompactString::new("m1"),
            timestamp: 1,
            tool_calls: vec![tc],
            images: Vec::new(),
        };
        s.messages.push(msg.clone());
        s.message_store
            .insert(compact_str::CompactString::new("m1"), msg);
        s.total_estimated_tokens = 1;
        s.schema_version = 1;
        // Apply migration.
        migrate_session(&mut s);
        // After migration: total reflects text + args + result + name + 16.
        assert!(
            s.total_estimated_tokens >= 1900,
            "migration must recompute estimates; got {}",
            s.total_estimated_tokens,
        );
        // Per-message field also corrected.
        assert!(s.messages[0].estimated_tokens >= 1900);
    }

    /// F8: pre-F8 session files (no `schema_version` field) load
    /// with `schema_version` defaulted to 0, then get migrated up
    /// to `SCHEMA_VERSION`. The migration is idempotent and
    /// transparent for current schema (no data shape changes
    /// between v0 and v1).
    #[test]
    fn load_session_migrates_pre_f8_files() {
        // Write a minimal pre-F8 session JSON without the
        // schema_version field to a temp session id, then load.
        let id = format!("dirge-test-load-{}", std::process::id());
        let dir = session_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.json", id));
        std::fs::write(
            &path,
            r#"{
                "id": "dirge-test-load-pre-f8",
                "name": "",
                "messages": [],
                "compactions": [],
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "total_tokens": 0,
                "total_cost": 0.0,
                "total_estimated_tokens": 0,
                "context_window": 100000,
                "model": "test-model",
                "provider": "test",
                "working_dir": "/tmp"
            }"#,
        )
        .unwrap();

        let result = load_session(&id);
        let _ = std::fs::remove_file(&path);

        let session = result.expect("pre-F8 file must load");
        assert_eq!(
            session.schema_version,
            crate::session::SCHEMA_VERSION,
            "migration must bump schema_version",
        );
        assert_eq!(session.model, "test-model");
    }

    /// F8: a truncated JSON file surfaces a CLEAR error mentioning
    /// the file path. Previously the user got
    /// `expected ',' or '}' at line N column M` with no file
    /// context, making it hard to identify which session was
    /// broken when many existed.
    #[test]
    fn load_session_corrupted_file_includes_path_in_error() {
        let id = format!("dirge-test-corrupt-{}", std::process::id());
        let dir = session_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.json", id));
        // Truncated JSON.
        std::fs::write(&path, r#"{"id": "x", "name":"#).unwrap();

        let err = load_session(&id).expect_err("truncated file must error");
        let _ = std::fs::remove_file(&path);

        let msg = format!("{:?}", err);
        assert!(
            msg.contains(&id) || msg.contains("failed to parse"),
            "error must reference the file: {msg}",
        );
    }

    #[test]
    fn validate_session_id_rejects_traversal() {
        assert!(validate_session_id("../../../etc/passwd").is_err());
        assert!(validate_session_id("..\\windows").is_err());
        assert!(validate_session_id("..").is_err());
        assert!(validate_session_id(".").is_err());
        assert!(validate_session_id("a/b").is_err());
        assert!(validate_session_id("a\\b").is_err());
        assert!(validate_session_id("").is_err());
        // Null bytes, newlines, spaces — anything non-id-shaped.
        assert!(validate_session_id("foo bar").is_err());
        assert!(validate_session_id("foo\nbar").is_err());
    }

    /// Batch2-3: when another writer's mtime is newer than ours
    /// at save time, the save diverts to a `.conflict-<ts>.json`
    /// sibling and returns an error so the UI surfaces a warning.
    /// The original on-disk file is preserved (so the other
    /// instance doesn't lose its work).
    #[test]
    fn save_session_diverts_to_conflict_on_concurrent_write() {
        use crate::session::Session;

        // Use a deterministic test id so cleanup is easy + tests
        // can run in parallel without colliding (each test thread
        // picks a unique id).
        let id = format!(
            "test-conflict-{}",
            std::process::id() as u64 * 1000
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos() as u64
        );
        let mut sess = Session::new("openrouter", "test-model", 128_000);
        sess.id = compact_str::CompactString::from(id.clone());

        // First write — establishes the on-disk file with mtime T0.
        save_session(&mut sess).expect("first save");

        // Simulate "loaded earlier": set loaded_mtime to T0 - 1s so
        // the on-disk mtime is necessarily newer. (We could also
        // sleep + re-save to advance the on-disk mtime; the sub-
        // second approach keeps the test fast.)
        sess.loaded_mtime = Some(std::time::SystemTime::now() - std::time::Duration::from_secs(60));

        // Second save with stale loaded_mtime — should detect the
        // newer on-disk file and divert.
        let result = save_session(&mut sess);
        assert!(result.is_err(), "expected conflict error");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("modified by another"), "got: {err_msg}");
        assert!(err_msg.contains(".conflict-"), "got: {err_msg}");

        // Cleanup: remove both the original + conflict files.
        let dir = session_dir();
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&format!("{id}.conflict-")))
                .unwrap_or(false)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    /// Fresh save (loaded_mtime = None) doesn't trigger the
    /// conflict check — first-write case must succeed.
    #[test]
    fn save_session_fresh_no_conflict_check() {
        use crate::session::Session;
        let id = format!(
            "test-fresh-{}",
            std::process::id() as u64 * 1000
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos() as u64
        );
        let mut sess = Session::new("openrouter", "test-model", 128_000);
        sess.id = compact_str::CompactString::from(id.clone());
        assert!(sess.loaded_mtime.is_none());
        save_session(&mut sess).expect("fresh save must succeed");
        let dir = session_dir();
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    }

    /// dirge-yrql contract: after a successful save, `loaded_mtime` tracks
    /// the file we just wrote. Deterministic catcher for the self-conflict
    /// bug — before the fix `save_session` took `&Session` and could not
    /// refresh it, so it stayed `None`/stale and the NEXT save mistook our
    /// own write for a concurrent instance.
    #[test]
    fn save_refreshes_loaded_mtime_to_written_file() {
        use crate::session::Session;
        let id = format!(
            "test-mtime-{}",
            std::process::id() as u64 * 1000
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos() as u64
        );
        let mut sess = Session::new("openrouter", "test-model", 128_000);
        sess.id = compact_str::CompactString::from(id.clone());
        let dir = session_dir();
        let path = dir.join(format!("{id}.json"));

        save_session(&mut sess).expect("save");
        let disk = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            sess.loaded_mtime,
            Some(disk),
            "loaded_mtime must be refreshed to the just-written file's mtime",
        );
        let _ = std::fs::remove_file(&path);
    }

    /// dirge-yrql regression: a RESUMED session (loaded_mtime = Some) must
    /// keep persisting to its real file on every save — never self-conflict
    /// and divert to `.conflict-*` siblings.
    #[test]
    fn resumed_session_keeps_saving_without_self_conflict() {
        use crate::session::{MessageRole, Session};
        let id = format!(
            "test-resume-{}",
            std::process::id() as u64 * 1000
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos() as u64
        );
        let mut sess = Session::new("openrouter", "test-model", 128_000);
        sess.id = compact_str::CompactString::from(id.clone());
        let dir = session_dir();
        let path = dir.join(format!("{id}.json"));

        // Seed the file, then LOAD it so loaded_mtime is Some (a resume).
        save_session(&mut sess).expect("seed save");
        let mut resumed = load_session(&id).expect("load");
        assert!(resumed.loaded_mtime.is_some(), "load records mtime");

        // Repeated saves of the resumed session must all succeed.
        for i in 0..3 {
            resumed.add_message(MessageRole::User, &format!("msg {i}"));
            save_session(&mut resumed)
                .unwrap_or_else(|e| panic!("resumed save {i} must succeed; got: {e}"));
        }

        // No conflict siblings were created.
        let conflicts = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(&format!("{id}.conflict-")))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(
            conflicts, 0,
            "resumed saves must not divert to conflict files"
        );

        // The real file reflects the latest content.
        let reloaded = load_session(&id).expect("reload");
        assert!(
            reloaded
                .messages
                .iter()
                .any(|m| m.content.contains("msg 2")),
            "the real session file must hold the latest message",
        );

        let _ = std::fs::remove_file(&path);
    }

    // --- Integration: full persistence round-trips -------------------

    fn unique_test_id(prefix: &str) -> String {
        format!(
            "test-{prefix}-{}",
            std::process::id() as u64 * 1000
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos() as u64
        )
    }

    fn cleanup_session(id: &str) {
        let dir = session_dir();
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        // Also clean any conflict files.
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(id))
                .unwrap_or(false)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    /// Full cycle: new session → add messages → save → load → verify.
    #[test]
    fn roundtrip_session_with_messages_survives_save_and_load() {
        use crate::session::{MessageRole, Session};
        let id = unique_test_id("roundtrip-msgs");
        let mut s = Session::new("anthropic", "claude-opus", 200_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.add_message(MessageRole::User, "what is the answer?");
        s.add_message(MessageRole::Assistant, "the answer is 42");
        let orig_msg_count = s.messages.len();
        let orig_tokens = s.total_estimated_tokens;

        save_session(&mut s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.messages.len(), orig_msg_count);
        assert_eq!(loaded.messages[0].role, MessageRole::User);
        assert_eq!(loaded.messages[0].content.as_str(), "what is the answer?");
        assert_eq!(loaded.messages[1].role, MessageRole::Assistant);
        assert_eq!(loaded.messages[1].content.as_str(), "the answer is 42");
        assert_eq!(loaded.total_estimated_tokens, orig_tokens);
        assert_eq!(loaded.model, "claude-opus");
        assert_eq!(loaded.provider, "anthropic");
        assert_eq!(loaded.context_window, 200_000);
        assert!(loaded.loaded_mtime.is_some(), "load must record mtime");
    }

    /// Messages with structured tool calls must survive the round-trip.
    #[test]
    fn roundtrip_tool_calls_survive_save_and_load() {
        use crate::session::{MessageRole, Session, ToolCallEntry, ToolCallState};
        let id = unique_test_id("roundtrip-tools");
        let mut s = Session::new("openai", "gpt-4", 128_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.add_message(MessageRole::User, "read the file");
        s.add_message_with_tool_calls(
            MessageRole::Assistant,
            "let me check",
            vec![ToolCallEntry {
                id: "call_abc".to_string(),
                name: "read".to_string(),
                args: serde_json::json!({"path": "/tmp/data.txt"}),
                state: ToolCallState::Completed {
                    result: "hello world\n".to_string(),
                },
            }],
        );

        save_session(&mut s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        let last = loaded.messages.last().unwrap();
        assert_eq!(last.tool_calls.len(), 1);
        assert_eq!(last.tool_calls[0].id, "call_abc");
        assert_eq!(last.tool_calls[0].name, "read");
        match &last.tool_calls[0].state {
            ToolCallState::Completed { result } => {
                assert_eq!(result, "hello world\n");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Permission allowlist entries survive save/load.
    #[test]
    fn roundtrip_permission_allowlist_survives() {
        use crate::session::{PermissionAllowEntry, Session};
        let id = unique_test_id("roundtrip-perm");
        let mut s = Session::new("openrouter", "test", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.permission_allowlist.push(PermissionAllowEntry {
            tool: "bash".to_string(),
            pattern: "cargo *".to_string(),
        });
        s.permission_allowlist.push(PermissionAllowEntry {
            tool: "read".to_string(),
            pattern: "/tmp/**".to_string(),
        });

        save_session(&mut s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.permission_allowlist.len(), 2);
        assert_eq!(loaded.permission_allowlist[0].tool, "bash");
        assert_eq!(loaded.permission_allowlist[0].pattern, "cargo *");
        assert_eq!(loaded.permission_allowlist[1].tool, "read");
        assert_eq!(loaded.permission_allowlist[1].pattern, "/tmp/**");
    }

    /// Compaction records persist through save/load.
    #[test]
    fn roundtrip_compaction_persists() {
        use crate::session::{MessageRole, Session};
        let id = unique_test_id("roundtrip-compact");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.add_message(MessageRole::User, "long conversation part 1");
        s.add_message(MessageRole::Assistant, "reply 1");
        s.add_message(MessageRole::User, "long conversation part 2");
        s.add_message(MessageRole::Assistant, "reply 2");
        s.compress("summary of first 4 messages".to_string(), 4, 50);

        save_session(&mut s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.compactions.len(), 1);
        assert_eq!(
            loaded.compactions[0].summary.as_str(),
            "summary of first 4 messages"
        );
        assert_eq!(loaded.compactions[0].summarized_count, 4);
        // After compress: summary at index 0, then any remaining messages.
        assert_eq!(loaded.messages[0].role, MessageRole::System);
        assert!(
            loaded.messages[0]
                .content
                .contains("summary of first 4 messages")
        );
    }

    /// dirge-g1ze/-4kgk/-f67u: a compaction fold that rotates the session id
    /// (the production `context_compacted` handler) must survive save ->
    /// `load_session_tip`. Resuming the original (origin) id hops the fold
    /// chain forward to the tip and recovers the folded summary + the
    /// verbatim kept tail, with the right message count, and the origin/chain
    /// id still resolves to the tip. A unique origin keeps the shared
    /// per-process session dir scan from colliding with other tests.
    #[test]
    fn fold_persist_resume_roundtrip() {
        use crate::session::{MessageRole, Session};

        let origin = format!("origin-{}", uuid::Uuid::new_v4().simple());

        // Root of the chain: origin unset (effective_origin == own id), the
        // full original message list, older timestamp. This is the file a fold
        // leaves behind on disk.
        let mut root = Session::new("p", "m", 128_000);
        root.id = compact_str::CompactString::new(origin.clone());
        root.add_message(MessageRole::User, "the original ask");
        root.add_message(MessageRole::Assistant, "first reply");
        root.add_message(MessageRole::User, "follow up");
        root.add_message(MessageRole::Assistant, "second reply");
        root.add_message(MessageRole::User, "recent question");
        root.add_message(MessageRole::Assistant, "recent answer");
        root.updated_at = compact_str::CompactString::new("2026-01-01T00:00:00+00:00");
        save_session(&mut root).expect("save root");

        // The fold, mirroring the production handler: compress first, THEN
        // capture the origin, THEN rotate id + set origin_id, THEN save under
        // the new id. first_kept_index=2 drains [u1, a1]; the kept tail is
        // [u2, a2, u3, a3] plus the injected summary at index 0.
        let tip_id = format!("compacted-{}", uuid::Uuid::new_v4().simple());
        let mut tip = root.clone();
        tip.compress("FOLDED SUMMARY OF EARLY TURNS".to_string(), 2, 100);
        assert_eq!(
            tip.effective_origin(),
            origin,
            "origin_id unset means id is still the origin"
        );
        tip.origin_id = Some(compact_str::CompactString::new(origin.clone()));
        tip.id = compact_str::CompactString::new(tip_id.clone());
        tip.updated_at = compact_str::CompactString::new("2026-06-01T00:00:00+00:00");
        save_session(&mut tip).expect("save tip");

        // Resume the ORIGINAL id — load_session_tip must hop forward to the tip.
        let resumed = load_session_tip(&origin).expect("resume");
        assert_eq!(
            resumed.id.as_str(),
            tip_id,
            "resuming the origin must resolve to the chain tip"
        );
        assert_eq!(
            resumed.effective_origin(),
            origin,
            "the tip carries the conversation origin forward"
        );

        // Folded summary is message[0] (a System entry).
        assert_eq!(resumed.messages[0].role, MessageRole::System);
        assert!(
            resumed.messages[0]
                .content
                .contains("FOLDED SUMMARY OF EARLY TURNS"),
            "summary must survive the round-trip, got: {:?}",
            resumed.messages[0].content
        );

        // Summary + the 4 kept tail messages; the verbatim recent turn is
        // intact and the folded-out head did not leak back in.
        assert_eq!(
            resumed.messages.len(),
            5,
            "summary + 4 kept tail messages, got {}",
            resumed.messages.len()
        );
        assert!(
            resumed
                .messages
                .iter()
                .any(|m| m.content.contains("recent answer")),
            "the kept tail must retain the verbatim recent turn"
        );
        assert!(
            !resumed
                .messages
                .iter()
                .any(|m| m.content.contains("the original ask")),
            "the folded-out head must not reappear after reload"
        );

        assert_eq!(resumed.compactions.len(), 1);
        assert_eq!(resumed.compactions[0].summarized_count, 2);

        cleanup_session(&origin);
        cleanup_session(&tip_id);
    }

    /// Tree + message_store round-trip (fork/clone/switch surfaces
    /// the same branches after reload).
    #[test]
    fn roundtrip_tree_and_message_store_survives() {
        use crate::session::{MessageRole, Session};
        let id = unique_test_id("roundtrip-tree");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.add_message(MessageRole::User, "root question");
        s.add_message(MessageRole::Assistant, "first answer");
        let fork_target = s.messages[1].id.clone();
        s.fork_at(&fork_target).expect("fork");
        s.add_message(MessageRole::Assistant, "alternate answer");

        save_session(&mut s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        // Tree must contain all 3 messages' nodes (root + fork-original
        // + alternate). Messages shows only the active branch (2).
        assert_eq!(
            loaded.tree.entries.len(),
            3,
            "tree must hold all 3 nodes: got {}",
            loaded.tree.entries.len(),
        );
        assert_eq!(
            loaded.messages.len(),
            2,
            "active branch has 2 messages: got {}",
            loaded.messages.len(),
        );
        assert_eq!(
            loaded.message_store.len(),
            3,
            "store must hold all 3 messages (root + fork original + alternate): got {}",
            loaded.message_store.len(),
        );
        // Leaf points to current end.
        assert_eq!(loaded.tree.leaf_id.as_ref(), Some(&loaded.messages[1].id));
        // The original fork target is still in the store (not in messages).
        assert!(loaded.message_store.contains_key(&fork_target));
    }

    /// Plugin entries survive save/load round-trip.
    #[test]
    fn roundtrip_plugin_entries_survives() {
        use crate::session::Session;
        let id = unique_test_id("roundtrip-plugin");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.append_plugin_entry("bookmark", "save point 1", true);
        s.append_plugin_entry("stats", r#"{"tokens": 500}"#, false);

        save_session(&mut s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.extra_entries.len(), 2);
        assert_eq!(loaded.extra_entries[0].custom_type, "bookmark");
        assert_eq!(loaded.extra_entries[0].data, "save point 1");
        assert!(loaded.extra_entries[0].display);
        assert_eq!(loaded.extra_entries[1].custom_type, "stats");
        assert!(!loaded.extra_entries[1].display);
        // seq values are monotonic and unique.
        assert!(loaded.extra_entries[0].seq < loaded.extra_entries[1].seq);
        assert!(loaded.next_entry_seq >= 2);
    }

    /// Session metadata (schema_version, id, name, created_at) survives
    /// a double save — the second save updates updated_at but preserves
    /// the rest.
    #[test]
    fn roundtrip_resave_preserves_metadata() {
        use crate::session::Session;
        let id = unique_test_id("roundtrip-resave");
        let mut s = Session::new("openrouter", "test", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        let orig_created = s.created_at.clone();
        save_session(&mut s).expect("save");

        // Load, modify, re-save.
        let mut loaded = load_session(&id).expect("load");
        loaded.add_message(crate::session::MessageRole::User, "added after reload");
        save_session(&mut loaded).expect("resave");

        let reloaded = load_session(&id).expect("reload");
        cleanup_session(&id);

        assert_eq!(reloaded.id, loaded.id);
        assert_eq!(reloaded.created_at, orig_created);
        assert_eq!(reloaded.messages.len(), 1);
        assert_eq!(reloaded.messages[0].content.as_str(), "added after reload");
        // updated_at must advance (or at least be present).
        assert!(!reloaded.updated_at.is_empty());
    }

    /// Deleting a session then loading it returns an error with
    /// the file path in the message.
    #[test]
    fn delete_session_removes_file() {
        use crate::session::Session;
        let id = unique_test_id("roundtrip-delete");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        save_session(&mut s).expect("save");

        delete_session(&id).expect("delete");
        let err = load_session(&id).expect_err("load after delete must fail");
        cleanup_session(&id); // best-effort — file already gone

        let msg = format!("{:?}", err);
        assert!(
            msg.contains("o such file") || msg.contains("No such file"),
            "error must mention missing file: {msg}"
        );
    }

    /// dirge-vpma.13: deleting a session removes its whole fold chain
    /// (origin + all rotations), not just the named tip — otherwise a
    /// pre-fold rotation stays on disk and resurfaces as resumable. An
    /// unrelated session must survive.
    #[test]
    fn delete_session_removes_whole_fold_chain() {
        use crate::session::{MessageRole, Session};
        let origin = format!("origin-{}", uuid::Uuid::new_v4().simple());

        let mut old = Session::new("p", "m", 128_000);
        old.id = compact_str::CompactString::new(origin.clone());
        old.add_message(MessageRole::User, "the original ask");
        save_session(&mut old).unwrap();

        let tip_id = format!("compacted-{}", uuid::Uuid::new_v4().simple());
        let mut tip = Session::new("p", "m", 128_000);
        tip.id = compact_str::CompactString::new(tip_id.clone());
        tip.origin_id = Some(compact_str::CompactString::new(origin.clone()));
        tip.add_message(MessageRole::User, "newer state");
        save_session(&mut tip).unwrap();

        // Unrelated session (different origin) — must survive the delete.
        let other_id = format!("other-{}", uuid::Uuid::new_v4().simple());
        let mut other = Session::new("p", "m", 128_000);
        other.id = compact_str::CompactString::new(other_id.clone());
        save_session(&mut other).unwrap();

        // The UI deletes by the tip id (find_sessions_by_prefix dedups to it).
        delete_session(&tip_id).expect("delete");

        assert!(
            load_session(&origin).is_err(),
            "origin file must be deleted"
        );
        assert!(load_session(&tip_id).is_err(), "tip file must be deleted");
        assert!(
            load_session(&other_id).is_ok(),
            "unrelated session must survive"
        );

        cleanup_session(&other_id);
    }

    /// `write_asset` creates `<assets_dir>/<asset_id>.png` and the
    /// bytes round-trip. `assets_dir` is scoped to the session id.
    #[test]
    fn write_asset_persists_png() {
        use crate::agent::agent_loop::message::AssetId;
        use crate::session::Session;
        let id = unique_test_id("asset-write");
        let mut s = Session::new("p", "m", 0);
        s.id = compact_str::CompactString::from(id.clone());
        let asset = AssetId("img1".to_string());
        let bytes = [0x89, b'P', b'N', b'G'];
        let path = s.write_asset(&asset, &bytes).expect("write");
        assert!(path.ends_with("img1.png"), "{path:?}");
        assert_eq!(std::fs::read(&path).expect("read back"), bytes);
        assert!(s.assets_dir().ends_with(&id), "{:?}", s.assets_dir());
        let _ = std::fs::remove_dir_all(assets_dir_for(&id));
    }

    /// `delete_session` removes the session's assets dir wholesale.
    #[test]
    fn delete_session_removes_assets() {
        use crate::agent::agent_loop::message::AssetId;
        use crate::session::Session;
        let id = unique_test_id("asset-delete");
        let mut s = Session::new("p", "m", 0);
        s.id = compact_str::CompactString::from(id.clone());
        s.write_asset(&AssetId("a".to_string()), &[1, 2, 3])
            .expect("write");
        let assets = assets_dir_for(&id);
        assert!(assets.exists(), "asset dir created");
        save_session(&mut s).expect("save");
        delete_session(&id).expect("delete");
        assert!(!assets.exists(), "assets dir removed on delete: {assets:?}");
    }

    /// A pre-image session message (no `images` field) deserializes
    /// with images defaulting to empty.
    #[test]
    fn legacy_session_message_without_images_deserializes() {
        let json = serde_json::json!({
            "role": "user",
            "content": "hi",
            "estimated_tokens": 1,
            "id": "x",
            "timestamp": 0,
            "tool_calls": []
        });
        let m: crate::session::SessionMessage =
            serde_json::from_value(json).expect("deserialize legacy");
        assert!(m.images.is_empty(), "images default to empty");
    }

    /// Branch summaries survive save/load (Phase 4 — pruned subtree
    /// records).
    #[test]
    fn roundtrip_branch_summaries_survive() {
        use crate::session::{BranchSummary, Session};
        let id = unique_test_id("roundtrip-branch");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.branch_summaries.push(BranchSummary {
            root_id: compact_str::CompactString::from("root-1"),
            parent_id: compact_str::CompactString::from("parent-1"),
            message_count: 12,
            preview: "alternative approach...".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
        });

        save_session(&mut s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.branch_summaries.len(), 1);
        assert_eq!(loaded.branch_summaries[0].root_id, "root-1");
        assert_eq!(loaded.branch_summaries[0].parent_id, "parent-1");
        assert_eq!(loaded.branch_summaries[0].message_count, 12);
        assert_eq!(
            loaded.branch_summaries[0].preview,
            "alternative approach..."
        );
    }
}
