//! What to say when a SQLite file is not readable.
//!
//! Issue #769: a user's agent reported `Failed to demote entry: database
//! disk image is malformed` on every attempt to save a memory, and asked
//! what to do about it. Nothing in the message could tell them. dirge
//! keeps two databases that can produce it — the per-project
//! `.dirge/sessions/state.db`, which also holds session persistence, and
//! the global memory store — and the error named neither, so there was
//! not even a file to go and look at.
//!
//! Two things live here:
//!
//! - [`describe`] turns a `rusqlite::Error` into a line a person can act
//!   on. Corruption gets the database's path and the recovery steps;
//!   everything else is passed through unchanged, because a busy timeout
//!   or a constraint violation is not an invitation to rebuild your
//!   database.
//! - [`quick_check`] asks SQLite whether the file is intact, so
//!   corruption is found when a store is opened rather than at whatever
//!   write happens to touch a bad page — which, before this, meant
//!   finding out mid-session and then failing identically for every
//!   session afterwards.

use rusqlite::{Connection, ErrorCode};

/// Is this the error of a file SQLite cannot read as a database?
///
/// `DatabaseCorrupt` is a damaged page; `NotADatabase` is a file whose
/// header is not SQLite's at all (truncated to nothing, replaced by a
/// sync conflict, or encrypted underneath us). They need the same answer
/// from the user, so they get the same message.
fn is_unreadable(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase,
                ..
            },
            _,
        )
    )
}

/// The recovery steps, given wherever the file turned out to be.
///
/// Ordered deliberately: try to keep the contents first, and only then
/// the option that throws them away. `.recover` is SQLite's own salvage
/// path and reads what is still intact rather than refusing the file
/// wholesale.
fn recovery_advice(path: Option<&str>) -> String {
    match path {
        Some(p) => format!(
            "  the database is at {p}\n\
             \x20 recover what is readable: sqlite3 '{p}' '.recover' | sqlite3 '{p}.recovered'\n\
             \x20 or start over, losing its contents: move '{p}' aside and restart dirge\n\
             \x20 if that directory is on a network share or a sync folder (Dropbox, iCloud, \
             OneDrive), move the project off it — SQLite corrupts there"
        ),
        // An in-memory or otherwise anonymous connection. Rare, and there
        // is nothing to point at, but saying so beats a bare error.
        None => "  the database has no file on disk (in-memory connection)".to_string(),
    }
}

/// One line about what failed, and — when the file itself is the problem
/// — where it is and what to do.
///
/// `doing` is the caller's own description ("Failed to demote entry"),
/// kept so existing messages read as they always did up to the point
/// where there is something more useful to say.
pub(crate) fn describe(conn: &Connection, doing: &str, err: &rusqlite::Error) -> String {
    if !is_unreadable(err) {
        return format!("{doing}: {err}");
    }
    format!(
        "{doing}: {err}\n\
         This database is damaged; dirge cannot read or write it until it is \
         repaired or replaced.\n{}",
        recovery_advice(conn.path())
    )
}

/// Ask SQLite whether the file is intact.
///
/// `quick_check` rather than `integrity_check`: it skips the
/// UNIQUE-constraint verification, which is the expensive half, and still
/// reads every page. On the largest real `state.db` to hand (3.6 MB) it
/// costs about 30 ms — a fair price at open for not discovering the
/// damage twenty minutes into a session.
pub(crate) fn quick_check(conn: &Connection) -> Result<(), String> {
    // The pragma reports problems as ROWS, not as an error: an intact
    // database returns exactly one row reading "ok". A damaged one can
    // return many, and a badly damaged one fails the query outright — so
    // both shapes have to be handled.
    let verdict: Result<String, rusqlite::Error> =
        conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0));
    match verdict {
        Ok(v) if v == "ok" => Ok(()),
        Ok(v) => Err(format!(
            "This database is damaged; dirge cannot read or write it until it is \
             repaired or replaced.\n  SQLite reports: {v}\n{}",
            recovery_advice(conn.path())
        )),
        Err(e) => Err(describe(conn, "Integrity check failed", &e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Same shape as `session_db_tests::temp_db` — this crate has no
    /// `tempfile` dependency, and adding one for two tests is not worth
    /// it. Returns a directory that removes itself when the test ends.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "dirge-db-health-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst),
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The discrimination half, written first: an ordinary failure must
    /// NOT collect recovery advice. A busy timeout or a constraint
    /// violation telling someone to rebuild their database would be
    /// worse than the bare error it replaced.
    #[test]
    fn an_ordinary_error_is_passed_through_unchanged() {
        let conn = Connection::open_in_memory().unwrap();
        let err = conn
            .execute("SELECT * FROM a_table_that_does_not_exist", [])
            .unwrap_err();
        let msg = describe(&conn, "Failed to demote entry", &err);
        assert!(msg.starts_with("Failed to demote entry:"), "{msg}");
        assert!(!msg.contains("damaged"), "{msg}");
        assert!(!msg.contains("recover"), "{msg}");
        // And it stays one line, as it always was.
        assert_eq!(msg.lines().count(), 1, "{msg}");
    }

    #[test]
    fn a_file_that_is_not_a_database_says_where_it_is_and_what_to_do() {
        let dir = Scratch::new("notadb");
        let path = dir.path().join("state.db");
        std::fs::write(&path, b"this is not a database").unwrap();
        let conn = Connection::open(&path).unwrap();

        let err = conn.execute("CREATE TABLE t (a)", []).unwrap_err();
        assert!(
            is_unreadable(&err),
            "expected an unreadable-file error: {err}"
        );

        let msg = describe(&conn, "Failed to demote entry", &err);
        assert!(msg.contains("damaged"), "{msg}");
        assert!(
            msg.contains(path.to_str().unwrap()),
            "the message must name the file: {msg}"
        );
        assert!(msg.contains(".recover"), "{msg}");
        assert!(
            msg.contains("sync folder"),
            "the likeliest cause is worth naming: {msg}"
        );
    }

    #[test]
    fn an_intact_database_passes_the_check() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE t (a)", []).unwrap();
        assert_eq!(quick_check(&conn), Ok(()));
    }

    #[test]
    fn a_damaged_database_fails_the_check_with_the_path_and_the_steps() {
        let dir = Scratch::new("damaged");
        let path = dir.path().join("state.db");
        // A real database, then a page scribbled over — the shape that
        // produces "database disk image is malformed" rather than the
        // cruder "file is not a database".
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = DELETE;
                 CREATE TABLE t (a TEXT);
                 CREATE INDEX t_a ON t (a);",
            )
            .unwrap();
            let mut stmt = conn.prepare("INSERT INTO t (a) VALUES (?1)").unwrap();
            for i in 0..500 {
                stmt.execute([format!("row {i} with enough text to fill some pages")])
                    .unwrap();
            }
        }
        let mut bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > 8192, "need more than one page to damage");
        // Past the header, into the b-tree.
        for b in bytes.iter_mut().skip(4096).take(2048) {
            *b = 0x5a;
        }
        std::fs::write(&path, &bytes).unwrap();

        let conn = Connection::open(&path).unwrap();
        let err = quick_check(&conn).expect_err("a scribbled-over database must not pass");
        assert!(err.contains("damaged"), "{err}");
        assert!(
            err.contains(path.to_str().unwrap()),
            "the message must name the file: {err}"
        );
        assert!(err.contains(".recover"), "{err}");
    }

    /// An anonymous connection has nothing to point at. It must still say
    /// what is wrong rather than printing an empty path.
    #[test]
    fn a_connection_with_no_file_still_explains_itself() {
        let advice = recovery_advice(None);
        assert!(advice.contains("in-memory"), "{advice}");
        assert!(!advice.contains("move ''"), "{advice}");
    }
}
