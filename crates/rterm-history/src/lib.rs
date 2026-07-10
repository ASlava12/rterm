//! Persistent terminal-side command history.
//!
//! rterm captures every command the user submits to a PTY (between
//! newlines on the input side) and stores it here. The store backs
//! the smart-autocomplete popup: when the user types a prefix, the
//! renderer queries `History::suggest(prefix, N)` and surfaces the
//! top-N matches ranked by frequency + recency.
//!
//! Key property: the capture lives in the renderer (the byte stream
//! we send to the PTY), NOT in the remote shell. Even an SSH session
//! to a stripped-down server with no `.bashrc` integration contributes
//! to history, because we see what the user typed regardless of where
//! the remote pty ends up running it.
//!
//! Storage is SQLite (via `rusqlite` with the `bundled` feature so the
//! binary has no system-SQLite dependency). The schema is intentionally
//! flat — one row per unique command text, with `count` and
//! `last_used` keeping ranking cheap.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE commands (
//!     text       TEXT NOT NULL,
//!     count      INTEGER NOT NULL DEFAULT 1,
//!     last_used  INTEGER NOT NULL,   -- unix seconds
//!     first_used INTEGER NOT NULL,   -- unix seconds
//!     context    TEXT NOT NULL DEFAULT '*',  -- per-profile / per-host bucket
//!     PRIMARY KEY (text, context)
//! );
//! ```
//! The composite key means the same command in two contexts (e.g. the
//! local `*` bucket and an `ssh-prod` profile bucket) is two rows, so
//! `suggest` can scope to a pane's context. Databases created by older
//! versions (single `text` PK) are migrated in place on open.
//!
//! ## Operations
//!
//! All public methods are blocking — the renderer calls them from the
//! UI thread. `suggest` filters on `text LIKE ?1 ESCAPE '\\'`, which
//! SQLite serves with a full-table scan + sort — the `ESCAPE` clause
//! defeats the prefix-index optimisation, so no secondary index helps
//! (an earlier `count`/`last_used` index pair only taxed every write
//! and is gone). Practical workloads (≤100k rows, prefix scans
//! returning ≤50) still complete in well under 1 ms; the renderer's
//! per-frame budget accommodates them without measurable cost.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// A single command-history entry returned from
/// [`History::suggest`]. Ordering inside a returned `Vec` is most-
/// frequent-first; ties broken by most-recently-used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The exact command text, byte-for-byte as the user submitted it
    /// (post-cleanup — see `clean_command` in the renderer).
    pub text: String,
    /// How many times this command has been submitted. The popup uses
    /// it to break ties when two suggestions share a prefix.
    pub count: u32,
    /// Unix seconds the command was most recently submitted.
    pub last_used: i64,
}

/// Per-process handle to the history store.
///
/// Multiple `History` handles against the same on-disk path are safe
/// for concurrent **readers** — SQLite serialises writes via its own
/// file lock and our schema doesn't require explicit transactions for
/// the single-statement `record` path. The renderer holds one handle
/// per `App` and reuses it for both `record` and `suggest`.
pub struct History {
    conn: Connection,
}

impl History {
    /// Open the history database at `path`, creating it (and the
    /// schema) if it doesn't exist. Pass `:memory:` for an
    /// ephemeral in-process store — convenient for tests and for
    /// the headless `--smoke` mode.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        if let Some(parent) = path_ref.parent() {
            // `parent()` is `Some("")` for `":memory:"` only on some
            // platforms — `create_dir_all("")` returns an error. Skip
            // for empty / in-memory paths.
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("mkdir {} (history dir)", parent.display())
                })?;
            }
        }
        let conn = Connection::open(path_ref).with_context(|| {
            format!("open history db at {}", path_ref.display())
        })?;
        // PRAGMA tuning. WAL is the right default for an
        // single-writer-many-reader workload; synchronous=NORMAL
        // gives a ~10× write speedup at the cost of losing the last
        // (~1 ms) write on a hard power loss, which is acceptable
        // for shell-history-grade data.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        // Two rterm instances (or the GUI plus `rterm --history …`)
        // share this file. Without a busy timeout a write collision
        // returns SQLITE_BUSY immediately and the command record is
        // silently dropped; a short retry window absorbs the overlap.
        conn.busy_timeout(std::time::Duration::from_millis(250)).ok();
        // Migrate an older single-`text`-PRIMARY-KEY table to the
        // composite `(text, context)` key so per-context (per-profile /
        // per-host) history buckets are distinct rows rather than one
        // shared row whose context is overwritten on each record. Detected
        // by the PK spanning exactly one column; the rebuild preserves all
        // existing rows (their context stays `*`).
        let old_single_pk = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('commands') WHERE pk > 0",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n == 1)
            .unwrap_or(false);
        if old_single_pk {
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE commands RENAME TO commands_old;
                 CREATE TABLE commands (
                     text       TEXT NOT NULL,
                     count      INTEGER NOT NULL DEFAULT 1,
                     last_used  INTEGER NOT NULL,
                     first_used INTEGER NOT NULL,
                     context    TEXT NOT NULL DEFAULT '*',
                     PRIMARY KEY (text, context)
                 );
                 INSERT INTO commands (text, count, last_used, first_used, context)
                     SELECT text, count, last_used, first_used, context FROM commands_old;
                 DROP TABLE commands_old;
                 COMMIT;",
            )
            .context("migrate history schema to composite key")?;
        }
        conn.execute_batch(
            // No secondary indexes: the only query (`text LIKE ?
            // ESCAPE '\\'` scoped by context) can't use them (see the
            // module doc), so the former `count`/`last_used` index pair
            // only taxed every write. `DROP INDEX IF EXISTS` cleans them
            // off DBs created by older versions.
            "CREATE TABLE IF NOT EXISTS commands (
                 text       TEXT NOT NULL,
                 count      INTEGER NOT NULL DEFAULT 1,
                 last_used  INTEGER NOT NULL,
                 first_used INTEGER NOT NULL,
                 context    TEXT NOT NULL DEFAULT '*',
                 PRIMARY KEY (text, context)
             );
             DROP INDEX IF EXISTS idx_commands_count;
             DROP INDEX IF EXISTS idx_commands_last_used;",
        )
        .context("init history schema")?;
        Ok(Self { conn })
    }

    /// Record a single command submission. `text` is the command line
    /// as the user typed it (no leading shell prompt, no trailing
    /// newline). Empty / whitespace-only commands are silently
    /// dropped — they're not useful to suggest.
    ///
    /// If the command already exists, `count` is incremented and
    /// `last_used` is bumped. The original `first_used` stays as-is,
    /// useful for "this command was first seen X months ago" stats.
    pub fn record(&self, text: &str, context: &str) -> Result<()> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let ctx = if context.is_empty() { "*" } else { context };
        let now = now_unix();
        // `ON CONFLICT (text, context) DO UPDATE` is the canonical
        // "upsert" SQLite spelling against the composite key — the same
        // command in a different context (profile / host bucket) is a
        // distinct row. Atomic in one statement.
        self.conn
            .execute(
                "INSERT INTO commands (text, count, last_used, first_used, context)
                 VALUES (?1, 1, ?2, ?2, ?3)
                 ON CONFLICT(text, context) DO UPDATE SET
                     count = count + 1,
                     last_used = excluded.last_used",
                params![trimmed, now, ctx],
            )
            .context("record command")?;
        Ok(())
    }

    /// Return up to `limit` history entries whose `text` starts with
    /// `prefix`. Empty prefix returns the global top-N (useful when
    /// the popup opens before the user starts typing). Sort order:
    /// `count DESC, last_used DESC, text ASC` — frequency wins,
    /// recency breaks ties, alphabetical breaks the rest so the list
    /// is stable across queries.
    pub fn suggest(&self, prefix: &str, limit: usize, context: &str) -> Result<Vec<Suggestion>> {
        // Cap the limit before it is used as an allocation hint and as
        // an SQL parameter: `Vec::with_capacity(usize::MAX)` aborts
        // with "capacity overflow", and `usize::MAX as i64` wraps to
        // -1, which SQLite reads as "no limit". Reachable from the
        // unvalidated `--history list --limit N` CLI flag.
        let limit = limit.min(10_000);
        // SQLite's LIKE pattern with `\` escape: replace any LIKE-
        // special bytes in the prefix with their escaped form so a
        // user typing `git_` doesn't suddenly match `git-status` /
        // `gita-status` / etc. ASCII `\` precedes the literal byte.
        let pattern = like_escape(prefix) + "%";
        let ctx = if context.is_empty() { "*" } else { context };
        // Scope suggestions to the pane's context bucket so a profile /
        // SSH pane sees its own history, isolated from the local `*`
        // bucket. A fresh bucket simply starts empty and fills up.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT text, count, last_used
                 FROM commands
                 WHERE context = ?3 AND text LIKE ?1 ESCAPE '\\'
                 ORDER BY count DESC, last_used DESC, text ASC
                 LIMIT ?2",
            )
            .context("prepare suggest")?;
        let rows = stmt
            .query_map(params![pattern, limit as i64, ctx], |row| {
                Ok(Suggestion {
                    text: row.get(0)?,
                    count: row.get::<_, i64>(1)? as u32,
                    last_used: row.get(2)?,
                })
            })
            .context("query suggest")?;
        let mut out = Vec::with_capacity(limit);
        for r in rows {
            out.push(r.context("decode suggest row")?);
        }
        Ok(out)
    }

    /// Total number of unique commands stored. Useful for tests and
    /// for a future "settings overlay" line that surfaces the count.
    pub fn len(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM commands", [], |r| r.get(0))
            .context("count commands")?;
        Ok(n as u64)
    }

    /// `true` when the store has no recorded commands. Conventional
    /// counterpart to `len`.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Look up a single command. Returns `None` if no entry matches.
    /// Mostly used by tests; the popup goes through `suggest`.
    pub fn lookup(&self, text: &str) -> Result<Option<Suggestion>> {
        self.conn
            .query_row(
                "SELECT text, count, last_used FROM commands WHERE text = ?1",
                params![text],
                |row| {
                    Ok(Suggestion {
                        text: row.get(0)?,
                        count: row.get::<_, i64>(1)? as u32,
                        last_used: row.get(2)?,
                    })
                },
            )
            .optional()
            .context("lookup command")
    }

    /// Drop every row. Used by tests and by a future "clear history"
    /// CLI / palette action.
    pub fn clear(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM commands", [])
            .context("clear history")?;
        Ok(())
    }

    /// Drop a specific entry. Returns `true` if a row was removed.
    pub fn forget(&self, text: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM commands WHERE text = ?1", params![text])
            .context("forget command")?;
        Ok(n > 0)
    }
}

/// Escape a string for safe use as a LIKE pattern prefix. Replaces
/// `%`, `_`, and `\` with the escaped form. Tests pin both the
/// escape character contract and the per-byte mapping.
fn like_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

/// Current Unix seconds. Falls back to 0 if the system clock is
/// before the epoch (impossible on real hardware but defensible).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_mem() -> History {
        // `:memory:` is SQLite's reserved name for an ephemeral
        // in-process database. Tests can use it freely without
        // touching the filesystem.
        History::open(":memory:").expect("open in-memory history")
    }

    #[test]
    fn suggest_is_scoped_by_context() {
        let h = open_mem();
        h.record("prod-cmd", "ssh-prod").unwrap();
        h.record("local-cmd", "*").unwrap();
        // The same text in two contexts is two distinct rows.
        h.record("shared", "*").unwrap();
        h.record("shared", "ssh-prod").unwrap();

        let prod: Vec<String> = h
            .suggest("", 10, "ssh-prod")
            .unwrap()
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert!(prod.contains(&"prod-cmd".to_string()));
        assert!(prod.contains(&"shared".to_string()));
        assert!(!prod.contains(&"local-cmd".to_string()), "local bucket stays isolated");

        let local: Vec<String> = h
            .suggest("", 10, "*")
            .unwrap()
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert!(local.contains(&"local-cmd".to_string()));
        assert!(local.contains(&"shared".to_string()));
        assert!(!local.contains(&"prod-cmd".to_string()));
    }

    #[test]
    fn migrates_old_single_pk_schema_preserving_rows() {
        let path = std::env::temp_dir().join(format!(
            "rterm-hist-migrate-{}-{}.db",
            std::process::id(),
            "t"
        ));
        let _ = std::fs::remove_file(&path);
        // Build a DB with the OLD single-`text`-PRIMARY-KEY schema + a row.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE commands (
                     text       TEXT PRIMARY KEY NOT NULL,
                     count      INTEGER NOT NULL DEFAULT 1,
                     last_used  INTEGER NOT NULL,
                     first_used INTEGER NOT NULL,
                     context    TEXT NOT NULL DEFAULT '*'
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO commands (text, count, last_used, first_used, context)
                 VALUES ('old cmd', 3, 100, 50, '*')",
                [],
            )
            .unwrap();
        }
        // Opening via `History` migrates the schema in place.
        let h = History::open(&path).unwrap();
        let s = h.suggest("old", 10, "*").unwrap();
        assert_eq!(s.len(), 1, "migrated row survives");
        assert_eq!(s[0].text, "old cmd");
        assert_eq!(s[0].count, 3);
        // The composite key now works: same text, different context.
        h.record("old cmd", "ssh-prod").unwrap();
        assert_eq!(h.suggest("old", 10, "ssh-prod").unwrap().len(), 1);
        // The original `*` bucket is untouched.
        assert_eq!(h.suggest("old", 10, "*").unwrap()[0].count, 3);
        drop(h);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_then_lookup_round_trips() {
        let h = open_mem();
        h.record("ls -la", "*").unwrap();
        let entry = h.lookup("ls -la").unwrap().expect("entry exists");
        assert_eq!(entry.text, "ls -la");
        assert_eq!(entry.count, 1);
        // last_used is non-zero (we just set it).
        assert!(entry.last_used > 0);
    }

    #[test]
    fn suggest_with_huge_limit_does_not_panic() {
        // Regression: `--history list --limit 18446744073709551615`
        // aborted on `Vec::with_capacity(usize::MAX)` and the
        // `as i64` wrap turned the LIMIT into -1 (unbounded).
        let h = open_mem();
        h.record("ls", "*").unwrap();
        h.record("ls -la", "*").unwrap();
        let out = h.suggest("ls", usize::MAX, "*").expect("suggest");
        assert_eq!(out.len(), 2);
        // Zero limit stays zero rows, not "no limit".
        assert!(h.suggest("ls", 0, "*").expect("suggest").is_empty());
    }

    #[test]
    fn record_dedupes_and_increments_count() {
        let h = open_mem();
        for _ in 0..7 {
            h.record("git status", "*").unwrap();
        }
        let e = h.lookup("git status").unwrap().unwrap();
        assert_eq!(e.count, 7, "count should track all 7 submissions");
        // The unique-command count is still 1.
        assert_eq!(h.len().unwrap(), 1);
    }

    #[test]
    fn record_ignores_empty_and_whitespace_only() {
        let h = open_mem();
        h.record("", "*").unwrap();
        h.record("   ", "*").unwrap();
        h.record("\t\n  ", "*").unwrap();
        assert!(h.is_empty().unwrap(), "no rows should land for empty inputs");
    }

    #[test]
    fn record_trims_surrounding_whitespace() {
        // Capture path strips outer whitespace before recording so
        // "  ls\n  " and "ls" collapse into one row.
        let h = open_mem();
        h.record("  ls  ", "*").unwrap();
        h.record("ls", "*").unwrap();
        assert_eq!(h.len().unwrap(), 1);
        let e = h.lookup("ls").unwrap().unwrap();
        assert_eq!(e.count, 2);
    }

    #[test]
    fn suggest_sorts_by_count_then_recency_then_alpha() {
        // Three commands, identical prefix, distinct frequencies.
        let h = open_mem();
        for _ in 0..3 {
            h.record("git status", "*").unwrap();
        }
        for _ in 0..7 {
            h.record("git commit", "*").unwrap();
        }
        h.record("git log", "*").unwrap();
        let s = h.suggest("git ", 10, "*").unwrap();
        assert_eq!(s.len(), 3);
        // Highest count first.
        assert_eq!(s[0].text, "git commit");
        assert_eq!(s[1].text, "git status");
        assert_eq!(s[2].text, "git log");
    }

    #[test]
    fn suggest_respects_limit() {
        let h = open_mem();
        for i in 0..50 {
            h.record(&format!("cmd-{i}"), "*").unwrap();
        }
        let s = h.suggest("cmd-", 10, "*").unwrap();
        assert_eq!(s.len(), 10, "limit should clamp the result set");
    }

    #[test]
    fn suggest_empty_prefix_returns_global_top_n() {
        let h = open_mem();
        for _ in 0..2 {
            h.record("ls", "*").unwrap();
        }
        for _ in 0..5 {
            h.record("vim", "*").unwrap();
        }
        let s = h.suggest("", 10, "*").unwrap();
        assert_eq!(s.len(), 2);
        // `vim` has count 5, `ls` has count 2.
        assert_eq!(s[0].text, "vim");
        assert_eq!(s[1].text, "ls");
    }

    #[test]
    fn suggest_returns_only_matching_prefix() {
        let h = open_mem();
        h.record("ls", "*").unwrap();
        h.record("ls -la", "*").unwrap();
        h.record("vim", "*").unwrap();
        let s = h.suggest("ls", 10, "*").unwrap();
        assert_eq!(s.len(), 2);
        for entry in &s {
            assert!(entry.text.starts_with("ls"), "{:?}", entry.text);
        }
    }

    #[test]
    fn suggest_escapes_like_metacharacters() {
        // Without escaping, the SQL `_` wildcard would let `git ` (a
        // literal trailing space) match `git-status` etc. Pin the
        // contract that LIKE special bytes are escaped so a user's
        // input is treated as a literal prefix.
        let h = open_mem();
        h.record("git-status", "*").unwrap();
        h.record("git_alias", "*").unwrap();
        let s = h.suggest("git_", 10, "*").unwrap();
        assert_eq!(s.len(), 1, "underscore must be literal, not wildcard");
        assert_eq!(s[0].text, "git_alias");
        // `%` literal in user input must also stay literal.
        h.record("100% done", "*").unwrap();
        let s = h.suggest("100%", 10, "*").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].text, "100% done");
    }

    #[test]
    fn like_escape_replaces_each_special_byte_with_backslash_prefix() {
        // Unit-test the helper directly so a regression that changes
        // the escape character (e.g. someone swapping to ESCAPE '$'
        // without updating the helper) gets caught here.
        assert_eq!(like_escape("plain"), "plain");
        assert_eq!(like_escape("a_b"), r"a\_b");
        assert_eq!(like_escape("a%b"), r"a\%b");
        assert_eq!(like_escape(r"a\b"), r"a\\b");
        // All three special bytes in one input.
        assert_eq!(like_escape(r"\%_"), r"\\\%\_");
    }

    #[test]
    fn clear_removes_every_row() {
        let h = open_mem();
        h.record("a", "*").unwrap();
        h.record("b", "*").unwrap();
        assert_eq!(h.len().unwrap(), 2);
        h.clear().unwrap();
        assert!(h.is_empty().unwrap());
    }

    #[test]
    fn forget_drops_only_the_named_entry() {
        let h = open_mem();
        h.record("keep", "*").unwrap();
        h.record("drop", "*").unwrap();
        assert!(h.forget("drop").unwrap());
        assert_eq!(h.len().unwrap(), 1);
        assert!(h.lookup("drop").unwrap().is_none());
        assert!(h.lookup("keep").unwrap().is_some());
        // Forgetting a non-existent entry returns false, no error.
        assert!(!h.forget("missing").unwrap());
    }

    #[test]
    fn record_updates_last_used_but_keeps_first_used() {
        // Hand-roll the timestamps so we can prove first_used is
        // sticky. We don't expose first_used yet but the schema
        // promise is real — pin it via direct SQL.
        let h = open_mem();
        h.conn
            .execute(
                "INSERT INTO commands (text, count, last_used, first_used)
                 VALUES ('cmd', 1, 1000, 1000)",
                [],
            )
            .unwrap();
        h.record("cmd", "*").unwrap();
        let (count, first, last): (i64, i64, i64) = h
            .conn
            .query_row(
                "SELECT count, first_used, last_used FROM commands WHERE text = 'cmd'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(first, 1000, "first_used must not move on subsequent records");
        assert!(last >= 1000, "last_used must be at least the seeded value");
    }

    #[test]
    fn open_creates_missing_parent_directory() {
        // The real path is `$XDG_CACHE_HOME/rterm/history.sqlite3` —
        // on a fresh install the `rterm/` directory doesn't exist
        // yet. `open` must `mkdir -p` it rather than fail with
        // ENOENT.
        let mut tmp = std::env::temp_dir();
        tmp.push(format!(
            "rterm-history-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        tmp.push("nested");
        tmp.push("history.sqlite3");
        let h = History::open(&tmp).expect("open with auto-mkdir");
        h.record("touch", "*").unwrap();
        assert_eq!(h.len().unwrap(), 1);
        drop(h);
        // Clean up the tree.
        let _ = std::fs::remove_dir_all(tmp.ancestors().nth(2).unwrap());
    }
}
