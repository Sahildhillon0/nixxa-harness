use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::executor::block_on;
use nixxa_contracts::conformance::store::run;
use nixxa_contracts::{
    AccountRef, Action, ActionId, ActionRecord, ActionRequest, Content, Message, NewSession, Observation, Outcome,
    ProviderId, Role, SandboxAction, Store, StoreError, Timestamp, Verdict, WorkspaceInfo,
};

use super::{SqliteStore, latest};

/// A fresh folder for one test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("nixxa-db-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("nixxa").join("nixxa.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn new_session(at: u64) -> NewSession {
    NewSession {
        workspace: WorkspaceInfo { name: "demo".into(), root: std::env::temp_dir().join("demo") },
        account: AccountRef::new("anthropic", "default"),
        started_at: Timestamp(at),
    }
}

fn user(text: &str) -> Message {
    Message::text(Role::User, text, Timestamp(1))
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_store_conforms_in_memory() {
    run(|| block_on(SqliteStore::in_memory()).expect("in-memory store")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_store_conforms_on_a_file() {
    let scratch = Scratch::new("conformance");
    let n = AtomicUsize::new(0);
    run(|| {
        let path = scratch.0.join(format!("db-{}.db", n.fetch_add(1, Ordering::SeqCst)));
        block_on(SqliteStore::open(path)).expect("file store")
    })
    .await;
}

#[tokio::test]
async fn the_bundled_sqlite_has_fts5() {
    let store = SqliteStore::in_memory().await.unwrap();
    let options = store
        .call(|conn| {
            let mut query = conn.prepare("PRAGMA compile_options").map_err(super::backend)?;
            let rows = query.query_map([], |row| row.get::<_, String>(0)).map_err(super::backend)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(super::backend)
        })
        .await
        .unwrap();
    assert!(options.iter().any(|o| o == "ENABLE_FTS5"), "{options:?}");
}

/// Everything a session holds, of every kind, comes back after the file is closed and opened
/// again by a new store.
#[tokio::test]
async fn reopening_the_file_returns_everything() {
    let scratch = Scratch::new("reopen");
    let request = ActionRequest {
        id: ActionId::new("toolu_1"),
        action: Action::Sandbox {
            action: SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None, background: false },
        },
        reason: Some("look".into()),
    };
    let observation = Observation::new(
        ActionId::new("toolu_1"),
        Outcome::Exec { exit_code: Some(0), stdout: "a.rs\n".into(), stderr: String::new() },
    );
    let messages = vec![
        user("list it"),
        Message {
            role: Role::Agent,
            content: Content::Reasoning {
                provider: ProviderId::new("anthropic"),
                data: serde_json::json!({ "type": "thinking", "thinking": "", "signature": "sig==", "n": 1.5 }),
            },
            at: Timestamp(2),
        },
        Message::text(Role::Agent, "Looking.", Timestamp(3)),
        Message { role: Role::Agent, content: Content::Action { request: request.clone() }, at: Timestamp(4) },
        Message {
            role: Role::Observation,
            content: Content::Observation { observation: observation.clone() },
            at: Timestamp(5),
        },
        Message { role: Role::Agent, content: Content::Summary { text: "listed".into() }, at: Timestamp(6) },
    ];
    let record = ActionRecord {
        request,
        verdict: Verdict::Allow,
        observation: Some(observation),
        checkpoint: Some(nixxa_contracts::CheckpointId::new("run-1.3")),
        at: Timestamp(5),
    };

    let id = {
        let store = SqliteStore::open(scratch.db()).await.unwrap();
        let info = store.create_session(new_session(100)).await.unwrap();
        for message in &messages {
            store.append_message(&info.id, message.clone()).await.unwrap();
        }
        store.append_action(&info.id, record.clone()).await.unwrap();
        info.id
    };

    let store = SqliteStore::open(scratch.db()).await.unwrap();
    let loaded = store.load_session(&id).await.unwrap();
    assert_eq!(loaded.messages, messages);
    assert_eq!(loaded.actions, vec![record]);
    assert_eq!(loaded.info.first_message.as_deref(), Some("list it"));
    assert_eq!(loaded.info.started_at, Timestamp(100));
    assert_eq!(store.list_sessions().await.unwrap(), vec![loaded.info]);
}

/// A plain connection, patient with a store thread still closing its own.
fn raw(path: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    conn
}

fn user_version(path: &Path) -> i64 {
    let conn = raw(path);
    conn.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap()
}

/// A new (or empty) file is migrated to the latest version; on Unix it is private.
#[tokio::test]
async fn an_empty_file_is_migrated() {
    let scratch = Scratch::new("migrate");
    std::fs::create_dir_all(scratch.db().parent().unwrap()).unwrap();
    std::fs::write(scratch.db(), b"").unwrap();
    let store = SqliteStore::open(scratch.db()).await.unwrap();
    store.create_session(new_session(1)).await.unwrap();
    drop(store);
    assert_eq!(user_version(&scratch.db()), latest());

    // Opening again applies nothing twice.
    let store = SqliteStore::open(scratch.db()).await.unwrap();
    assert_eq!(store.list_sessions().await.unwrap().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn a_new_file_is_readable_by_its_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("private");
    let store = SqliteStore::open(scratch.db()).await.unwrap();
    store.create_session(new_session(1)).await.unwrap();
    let mode = std::fs::metadata(scratch.db()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

/// A file from a newer Nixxa is refused with a clear error, and not changed at all.
#[tokio::test]
async fn a_newer_file_is_refused_and_left_alone() {
    let scratch = Scratch::new("newer");
    {
        let store = SqliteStore::open(scratch.db()).await.unwrap();
        store.create_session(new_session(1)).await.unwrap();
    }
    {
        let conn = raw(&scratch.db());
        conn.pragma_update(None, "user_version", latest() + 1).unwrap();
        // Everything into the main file before the snapshot. The first store closes its
        // connection on its own thread, possibly after this; once the WAL is empty its closing
        // checkpoint has nothing to write. Retry while that connection is still busy.
        loop {
            let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0)).unwrap();
            if busy == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let before = std::fs::read(scratch.db()).unwrap();
    let Err(StoreError::Backend(message)) = SqliteStore::open(scratch.db()).await else {
        panic!("a newer file must be refused")
    };
    assert!(message.contains("newer Nixxa") && message.contains(&scratch.db().display().to_string()), "{message}");
    assert_eq!(std::fs::read(scratch.db()).unwrap(), before, "the file must not change");
    assert_eq!(user_version(&scratch.db()), latest() + 1);
}

/// A file that isn't a database gives an error naming it, and is left exactly as it was.
#[tokio::test]
async fn a_corrupt_file_is_reported_and_left_alone() {
    let scratch = Scratch::new("corrupt");
    std::fs::create_dir_all(scratch.db().parent().unwrap()).unwrap();
    let garbage: Vec<u8> = (0..8192u32).map(|i| (i * 7 + 3) as u8).collect();
    std::fs::write(scratch.db(), &garbage).unwrap();
    let Err(StoreError::Backend(message)) = SqliteStore::open(scratch.db()).await else {
        panic!("a corrupt file must be an error")
    };
    assert!(message.contains(&scratch.db().display().to_string()), "names the file: {message}");
    assert!(message.contains("left as it is"), "{message}");
    assert_eq!(std::fs::read(scratch.db()).unwrap(), garbage, "the file must not change");
}

/// Two stores (two Nixxa instances) writing to one file at the same time both succeed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_connections_write_at_once() {
    let scratch = Scratch::new("two");
    let first = Arc::new(SqliteStore::open(scratch.db()).await.unwrap());
    let second = Arc::new(SqliteStore::open(scratch.db()).await.unwrap());
    let a = first.create_session(new_session(1)).await.unwrap().id;
    let b = second.create_session(new_session(2)).await.unwrap().id;
    let write = |store: Arc<SqliteStore>, mine: nixxa_contracts::SessionId, shared: nixxa_contracts::SessionId| {
        tokio::spawn(async move {
            for i in 0..100 {
                store.append_message(&mine, user(&format!("own {i}"))).await.unwrap();
                store.append_message(&shared, user(&format!("shared {i}"))).await.unwrap();
            }
        })
    };
    let (x, y) = (write(first.clone(), a.clone(), b.clone()), write(second.clone(), b.clone(), a.clone()));
    x.await.unwrap();
    y.await.unwrap();
    for (store, id) in [(&first, &a), (&second, &b)] {
        let messages = store.load_session(id).await.unwrap().messages;
        assert_eq!(messages.len(), 200);
        let own: Vec<_> = messages
            .iter()
            .filter_map(|m| match &m.content {
                Content::Text { text } if text.starts_with("own") => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(own, (0..100).map(|i| format!("own {i}")).collect::<Vec<_>>(), "one writer's order is kept");
    }
}

#[tokio::test]
async fn search_ranks_the_best_match_first_and_ignores_case() {
    let store = SqliteStore::in_memory().await.unwrap();
    let long = store.create_session(new_session(1)).await.unwrap().id;
    let short = store.create_session(new_session(2)).await.unwrap().id;
    let none = store.create_session(new_session(3)).await.unwrap().id;
    let filler = "word ".repeat(60);
    store.append_message(&long, user(&format!("{filler} a needle somewhere\n{filler}"))).await.unwrap();
    store.append_message(&short, user("the NEEDLE, the needle")).await.unwrap();
    store.append_message(&none, user("nothing to see")).await.unwrap();
    // Not searched: only text messages are.
    let summary = Message { role: Role::Agent, content: Content::Summary { text: "needle".into() }, at: Timestamp(1) };
    store.append_message(&none, summary).await.unwrap();

    let hits = store.search("NeEdLe").await.unwrap();
    let sessions: Vec<_> = hits.iter().map(|h| h.session.clone()).collect();
    assert_eq!(sessions, vec![short, long.clone()], "best match first, one hit per session");
    let snippet = &hits[1].snippet;
    assert!(snippet.contains("needle") && snippet.len() < 200, "short, around the match: {snippet:?}");
    assert!(!snippet.chars().any(char::is_control), "one line: {snippet:?}");

    assert_eq!(store.search("need").await.unwrap().len(), 2, "the last word may be a prefix");
    assert_eq!(store.search("needle nothing").await.unwrap().len(), 0, "every word must match");
    assert!(store.search("  ").await.unwrap().is_empty());
    for odd in ["\"", "needle AND (", "NEAR(x", "*", "a:b"] {
        assert!(store.search(odd).await.is_ok(), "typed text is never FTS5 syntax: {odd}");
    }
}

#[test]
fn snippets_are_one_line() {
    assert_eq!(super::one_line("a\nb\r\n\tc  \u{1b}[31md"), "a b c [31md");
}
