//! [`SqliteStore`]: Nixxa's sessions in one SQLite file (Stage 5).
//!
//! * One connection, owned by a dedicated thread: every call is sent to it as a job, so no
//!   blocking SQLite call ever runs on the async runtime. A dropped call's job still runs to the
//!   end, so a write is never left half-done.
//! * WAL mode and a busy timeout, so two Nixxa instances can use the same file.
//! * Forward-only migrations embedded in the binary, tracked with `PRAGMA user_version`, each
//!   in its own transaction. A file from a newer Nixxa is refused before anything is written.
//! * Messages and action records are stored as their exact JSON; only what is queried has a
//!   real column (D-054). Search is FTS5 over the text of `Content::Text` messages (D-055).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::oneshot;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use nixxa_contracts::{
    AccountRef, ActionRecord, Content, Message, NewSession, Role, SearchHit, SessionId, SessionInfo, SessionRecord,
    Store, StoreError, Timestamp, WorkspaceInfo,
};

/// How long a write waits for another Nixxa instance holding the database.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// At most this many sessions come back from one search.
const SEARCH_LIMIT: usize = 50;
/// Words of context in a search snippet.
const SNIPPET_WORDS: u32 = 12;

/// The schema, one forward-only step per entry. Step `n` (0-based) takes `user_version` from
/// `n` to `n + 1`. Never edit a released step; add a new one.
const MIGRATIONS: &[&str] = &[
    // 1: sessions, messages, action records, and the transcript index.
    "
    CREATE TABLE sessions (
        id            TEXT PRIMARY KEY,
        workspace     TEXT NOT NULL,     -- WorkspaceInfo as JSON
        account       TEXT NOT NULL,     -- AccountRef as JSON
        started_at    INTEGER NOT NULL,  -- ms since the epoch
        first_message TEXT               -- the first user text, for the session list
    );
    CREATE INDEX sessions_by_start ON sessions (started_at DESC);

    CREATE TABLE messages (
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,  -- append order
        session_id TEXT NOT NULL REFERENCES sessions (id),
        text       TEXT,                               -- set for Content::Text only (searched)
        body       TEXT NOT NULL                       -- the Message as JSON, exactly as given
    );
    CREATE INDEX messages_by_session ON messages (session_id, seq);

    CREATE TABLE actions (
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL REFERENCES sessions (id),
        body       TEXT NOT NULL                       -- the ActionRecord as JSON
    );
    CREATE INDEX actions_by_session ON actions (session_id, seq);

    -- Case-insensitive word search (unicode61 folds case and, with 2, diacritics).
    CREATE VIRTUAL TABLE messages_fts USING fts5 (
        text, content = 'messages', content_rowid = 'seq', tokenize = 'unicode61 remove_diacritics 2'
    );
    -- Messages are append-only, so inserting is the only change to mirror.
    CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages WHEN new.text IS NOT NULL BEGIN
        INSERT INTO messages_fts (rowid, text) VALUES (new.seq, new.text);
    END;
    ",
];

/// The schema version this build writes.
fn latest() -> i64 {
    MIGRATIONS.len() as i64
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// A [`Store`] in a SQLite file (or in memory, for tests).
pub struct SqliteStore {
    jobs: mpsc::Sender<Job>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore").finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Opens the database at `path`, creating it (and its folder) if needed, and brings its
    /// schema up to date. On Unix a new file is readable by its owner only.
    ///
    /// A file that isn't a database, or is damaged, or was written by a newer Nixxa, is an
    /// error naming the path; the file is left exactly as it was.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        Self::start(move || open_file(&path)).await
    }

    /// A private in-memory database, gone when the store is dropped.
    pub async fn in_memory() -> Result<Self, StoreError> {
        Self::start(|| {
            let mut conn = Connection::open_in_memory().map_err(backend)?;
            prepare(&mut conn, Path::new(":memory:"))?;
            Ok(conn)
        })
        .await
    }

    /// Starts the database thread, which opens the connection with `open` and then runs jobs
    /// until the store is dropped.
    async fn start(open: impl FnOnce() -> Result<Connection, StoreError> + Send + 'static) -> Result<Self, StoreError> {
        let (jobs, inbox) = mpsc::channel::<Job>();
        let (ready, opened) = oneshot::channel();
        std::thread::Builder::new()
            .name("nixxa-db".into())
            .spawn(move || {
                let mut conn = match open() {
                    Ok(conn) => {
                        let _ = ready.send(Ok(()));
                        conn
                    }
                    Err(err) => {
                        let _ = ready.send(Err(err));
                        return;
                    }
                };
                while let Ok(job) = inbox.recv() {
                    job(&mut conn);
                }
            })
            .map_err(|err| StoreError::Backend(format!("could not start the database thread: {err}")))?;
        opened.await.map_err(|_| closed())??;
        Ok(Self { jobs })
    }

    /// Runs `f` on the database thread and waits for its answer.
    async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let (reply, answer) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = reply.send(f(conn));
        });
        self.jobs.send(job).map_err(|_| closed())?;
        answer.await.map_err(|_| closed())?
    }
}

fn closed() -> StoreError {
    StoreError::Backend("the database thread stopped".into())
}

fn backend(err: rusqlite::Error) -> StoreError {
    StoreError::Backend(err.to_string())
}

fn json_err(err: serde_json::Error) -> StoreError {
    StoreError::Backend(format!("a stored record could not be read or written: {err}"))
}

/// Creates the file with owner-only permissions if it doesn't exist, then opens and prepares it.
fn open_file(path: &Path) -> Result<Connection, StoreError> {
    let shown = path.display();
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|err| StoreError::Backend(format!("could not create {}: {err}", dir.display())))?;
    }
    create_private(path).map_err(|err| StoreError::Backend(format!("could not create {shown}: {err}")))?;
    let mut conn =
        Connection::open(path).map_err(|err| StoreError::Backend(format!("could not open {shown}: {err}")))?;
    prepare(&mut conn, path)?;
    Ok(conn)
}

/// Creates an empty file readable by its owner only; an existing file is left untouched.
#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other.map(drop),
    }
}

/// Creates an empty file (it inherits the per-user data folder's permissions); an existing file
/// is left untouched.
#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<()> {
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other.map(drop),
    }
}

/// Checks the file is a database this build can use, then sets the connection up and migrates.
/// Nothing is written before the version check passes.
fn prepare(conn: &mut Connection, path: &Path) -> Result<(), StoreError> {
    let shown = path.display();
    conn.busy_timeout(BUSY_TIMEOUT).map_err(backend)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(|err| {
        StoreError::Backend(format!(
            "{shown} can't be read as Nixxa's history ({err}). It was left as it is; move it away to start a new history."
        ))
    })?;
    refuse_newer(version, path)?;
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(())).map_err(backend)?;
    conn.pragma_update(None, "synchronous", "NORMAL").map_err(backend)?;
    conn.pragma_update(None, "foreign_keys", true).map_err(backend)?;
    migrate(conn, path)
}

fn refuse_newer(version: i64, path: &Path) -> Result<(), StoreError> {
    if version > latest() {
        return Err(StoreError::Backend(format!(
            "{} was written by a newer Nixxa (history format {version}; this one reads up to {}). \
             It was left as it is; update Nixxa to use it.",
            path.display(),
            latest()
        )));
    }
    Ok(())
}

/// Applies every step the file hasn't had, each in its own transaction. The version is read
/// again inside the transaction, so two instances opening a new file at once migrate it once.
fn migrate(conn: &mut Connection, path: &Path) -> Result<(), StoreError> {
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(backend)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(backend)?;
        refuse_newer(version, path)?;
        let Some(step) = usize::try_from(version).ok().and_then(|v| MIGRATIONS.get(v)) else {
            return Ok(());
        };
        tx.execute_batch(step)
            .map_err(|err| StoreError::Backend(format!("updating {} failed: {err}", path.display())))?;
        tx.pragma_update(None, "user_version", version + 1).map_err(backend)?;
        tx.commit().map_err(backend)?;
    }
}

fn not_found(id: &SessionId) -> StoreError {
    StoreError::NotFound(id.clone())
}

fn timestamp(value: i64) -> Timestamp {
    Timestamp(u64::try_from(value).unwrap_or_default())
}

fn millis(at: Timestamp) -> i64 {
    i64::try_from(at.0).unwrap_or(i64::MAX)
}

/// Reads one `sessions` row (`id, workspace, account, started_at, first_message`).
fn session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, String, i64, Option<String>)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
}

fn session_info(
    (id, workspace, account, started_at, first_message): (String, String, String, i64, Option<String>),
) -> Result<SessionInfo, StoreError> {
    Ok(SessionInfo {
        id: SessionId::new(id),
        workspace: serde_json::from_str::<WorkspaceInfo>(&workspace).map_err(json_err)?,
        account: serde_json::from_str::<AccountRef>(&account).map_err(json_err)?,
        started_at: timestamp(started_at),
        first_message,
    })
}

const SESSION_COLUMNS: &str = "id, workspace, account, started_at, first_message";

/// A snippet made safe to draw on one line: control characters (newlines included) become
/// spaces, and runs of whitespace collapse.
fn one_line(snippet: &str) -> String {
    let spaced: String = snippet.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Turns what the user typed into an FTS5 query: every word must appear, the last one may be
/// the start of a word (so results follow typing), and nothing is read as FTS5 syntax.
fn fts_query(typed: &str) -> Option<String> {
    let words: Vec<String> =
        typed.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| format!("\"{w}\"")).collect();
    let last = words.len().checked_sub(1)?;
    let mut query = words;
    query[last].push('*');
    Some(query.join(" "))
}

#[async_trait]
impl Store for SqliteStore {
    async fn create_session(&self, new: NewSession) -> Result<SessionInfo, StoreError> {
        let workspace = serde_json::to_string(&new.workspace).map_err(json_err)?;
        let account = serde_json::to_string(&new.account).map_err(json_err)?;
        self.call(move |conn| {
            let id: String = conn
                .query_row(
                    "INSERT INTO sessions (id, workspace, account, started_at)
                     VALUES (lower(hex(randomblob(8))), ?1, ?2, ?3) RETURNING id",
                    params![workspace, account, millis(new.started_at)],
                    |row| row.get(0),
                )
                .map_err(backend)?;
            Ok(SessionInfo {
                id: SessionId::new(id),
                workspace: new.workspace,
                account: new.account,
                started_at: new.started_at,
                first_message: None,
            })
        })
        .await
    }

    async fn load_session(&self, id: &SessionId) -> Result<SessionRecord, StoreError> {
        let id = id.clone();
        self.call(move |conn| {
            // One read transaction: the messages and actions are from the same moment.
            let tx = conn.transaction().map_err(backend)?;
            let row = tx
                .query_row(&format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"), [id.as_str()], session_row)
                .optional()
                .map_err(backend)?
                .ok_or_else(|| not_found(&id))?;
            let info = session_info(row)?;
            let mut messages = Vec::new();
            let mut query =
                tx.prepare("SELECT body FROM messages WHERE session_id = ?1 ORDER BY seq").map_err(backend)?;
            for body in query.query_map([id.as_str()], |row| row.get::<_, String>(0)).map_err(backend)? {
                messages.push(serde_json::from_str::<Message>(&body.map_err(backend)?).map_err(json_err)?);
            }
            let mut actions = Vec::new();
            let mut query =
                tx.prepare("SELECT body FROM actions WHERE session_id = ?1 ORDER BY seq").map_err(backend)?;
            for body in query.query_map([id.as_str()], |row| row.get::<_, String>(0)).map_err(backend)? {
                actions.push(serde_json::from_str::<ActionRecord>(&body.map_err(backend)?).map_err(json_err)?);
            }
            Ok(SessionRecord { info, messages, actions })
        })
        .await
    }

    async fn append_message(&self, id: &SessionId, message: Message) -> Result<(), StoreError> {
        let id = id.clone();
        let body = serde_json::to_string(&message).map_err(json_err)?;
        let text = match message.content {
            Content::Text { text } => Some(text),
            _ => None,
        };
        let first = text.clone().filter(|_| message.role == Role::User);
        self.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(backend)?;
            let added = tx
                .execute(
                    "INSERT INTO messages (session_id, text, body)
                     SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ?1)",
                    params![id.as_str(), text, body],
                )
                .map_err(backend)?;
            if added == 0 {
                return Err(not_found(&id));
            }
            if let Some(first) = first {
                tx.execute(
                    "UPDATE sessions SET first_message = ?2 WHERE id = ?1 AND first_message IS NULL",
                    params![id.as_str(), first],
                )
                .map_err(backend)?;
            }
            tx.commit().map_err(backend)
        })
        .await
    }

    async fn append_action(&self, id: &SessionId, record: ActionRecord) -> Result<(), StoreError> {
        let id = id.clone();
        let body = serde_json::to_string(&record).map_err(json_err)?;
        self.call(move |conn| {
            let added = conn
                .execute(
                    "INSERT INTO actions (session_id, body)
                     SELECT ?1, ?2 WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ?1)",
                    params![id.as_str(), body],
                )
                .map_err(backend)?;
            if added == 0 { Err(not_found(&id)) } else { Ok(()) }
        })
        .await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, StoreError> {
        self.call(|conn| {
            let mut query = conn
                .prepare(&format!("SELECT {SESSION_COLUMNS} FROM sessions ORDER BY started_at DESC, rowid DESC"))
                .map_err(backend)?;
            let rows = query.query_map([], session_row).map_err(backend)?;
            rows.map(|row| session_info(row.map_err(backend)?)).collect()
        })
        .await
    }

    /// Best match first, one hit per session (its best-matching message).
    async fn search(&self, query: &str) -> Result<Vec<SearchHit>, StoreError> {
        let Some(query) = fts_query(query) else { return Ok(Vec::new()) };
        self.call(move |conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT m.session_id, snippet(messages_fts, 0, '', '', '…', {SNIPPET_WORDS})
                     FROM messages_fts JOIN messages m ON m.seq = messages_fts.rowid
                     WHERE messages_fts MATCH ?1 ORDER BY rank"
                ))
                .map_err(backend)?;
            let rows = statement
                .query_map([query], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .map_err(backend)?;
            let mut hits: Vec<SearchHit> = Vec::new();
            for row in rows {
                let (session, snippet) = row.map_err(backend)?;
                if hits.iter().any(|hit| hit.session.as_str() == session) {
                    continue;
                }
                hits.push(SearchHit { session: SessionId::new(session), snippet: one_line(&snippet) });
                if hits.len() == SEARCH_LIMIT {
                    break;
                }
            }
            Ok(hits)
        })
        .await
    }
}

#[cfg(test)]
mod tests;
