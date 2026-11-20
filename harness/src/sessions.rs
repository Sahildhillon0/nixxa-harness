//! The session list (`ctrl-s`, D-059): stored sessions, newest first, the ones started in this
//! directory first. Typing filters the list by start message, workspace and account, and by
//! transcript text through `Store::search`, whose snippets are shown under the matching rows.
//! No I/O here: the TUI loop lists and searches, and feeds the answers back.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nixxa_contracts::{SearchHit, SessionId, SessionInfo, Timestamp};

use crate::app::types_text;

/// What a key in the list asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListKey {
    Nothing,
    Close,
    /// Search transcripts for this text.
    Search(String),
    /// Resume this session.
    Resume(SessionId),
    /// Enter on the session that is already open.
    AlreadyOpen,
}

/// One row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row<'a> {
    pub(crate) info: &'a SessionInfo,
    /// Started in the directory Nixxa runs in.
    pub(crate) here: bool,
    /// The session open now.
    pub(crate) open: bool,
    /// Where the typed text matched in the transcript.
    pub(crate) snippet: Option<&'a str>,
}

/// The list's state.
#[derive(Debug)]
pub(crate) struct SessionList {
    here: PathBuf,
    current: Option<SessionId>,
    /// `None` while loading; `Err` when the store couldn't list.
    sessions: Option<Result<Vec<SessionInfo>, String>>,
    pub(crate) query: String,
    /// Transcript matches, and the query they are for.
    hits: (String, Vec<SearchHit>),
    pub(crate) search_error: Option<String>,
    pub(crate) selected: usize,
}

impl SessionList {
    pub(crate) fn new(here: &Path, current: Option<SessionId>) -> Self {
        Self {
            here: here.to_path_buf(),
            current,
            sessions: None,
            query: String::new(),
            hits: (String::new(), Vec::new()),
            search_error: None,
            selected: 0,
        }
    }

    /// The stored sessions arrived (newest first), or why not.
    pub(crate) fn listed(&mut self, result: Result<Vec<SessionInfo>, String>) {
        self.sessions = Some(result);
        self.clamp();
    }

    /// Transcript matches for `query` arrived. Answers for an older query are dropped.
    pub(crate) fn found(&mut self, query: &str, result: Result<Vec<SearchHit>, String>) {
        if query != self.query.trim() {
            return;
        }
        match result {
            Ok(hits) => {
                self.hits = (query.to_string(), hits);
                self.search_error = None;
            }
            Err(why) => self.search_error = Some(why),
        }
        self.clamp();
    }

    pub(crate) fn loading(&self) -> bool {
        self.sessions.is_none()
    }

    pub(crate) fn list_error(&self) -> Option<&str> {
        match &self.sessions {
            Some(Err(why)) => Some(why),
            _ => None,
        }
    }

    /// The rows to show: this directory's sessions, then the others, each newest first.
    pub(crate) fn rows(&self) -> Vec<Row<'_>> {
        let Some(Ok(sessions)) = &self.sessions else { return Vec::new() };
        let query = self.query.trim().to_lowercase();
        let hits: &[SearchHit] = if self.hits.0 == self.query.trim() { &self.hits.1 } else { &[] };
        let mut rows: Vec<Row<'_>> = sessions
            .iter()
            .filter_map(|info| {
                let snippet = hits.iter().find(|h| h.session == info.id).map(|h| h.snippet.as_str());
                let listed = query.is_empty() || snippet.is_some() || describes(info, &query);
                listed.then(|| Row {
                    info,
                    here: info.workspace.root == self.here,
                    open: self.current.as_ref() == Some(&info.id),
                    snippet,
                })
            })
            .collect();
        // Stable: newest first within each group.
        rows.sort_by_key(|row| !row.here);
        rows
    }

    /// Handles one key.
    pub(crate) fn key(&mut self, key: KeyEvent) -> ListKey {
        match key.code {
            KeyCode::Esc => return ListKey::Close,
            KeyCode::Enter => {
                return match self.rows().get(self.selected) {
                    Some(row) if row.open => ListKey::AlreadyOpen,
                    Some(row) => ListKey::Resume(row.info.id.clone()),
                    None => ListKey::Nothing,
                };
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected += 1,
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                return self.edited();
            }
            KeyCode::Backspace => {
                self.query.pop();
                return self.edited();
            }
            KeyCode::Char(c) if types_text(&key) => {
                self.query.push(c);
                return self.edited();
            }
            _ => {}
        }
        self.clamp();
        ListKey::Nothing
    }

    /// The query changed: back to the top, and search transcripts unless it is blank.
    fn edited(&mut self) -> ListKey {
        self.selected = 0;
        self.search_error = None;
        let query = self.query.trim();
        if query.is_empty() { ListKey::Nothing } else { ListKey::Search(query.to_string()) }
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.rows().len().saturating_sub(1));
    }
}

/// Whether the row's own text (start message, workspace, account) contains `query` (lowercase).
fn describes(info: &SessionInfo, query: &str) -> bool {
    let first = info.first_message.as_deref().unwrap_or_default();
    [first, info.workspace.name.as_str(), &info.workspace.root.display().to_string(), &info.account.to_string()]
        .iter()
        .any(|field| field.to_lowercase().contains(query))
}

/// How long ago `then` was, in a few words. Relative, so it needs no time zone.
pub(crate) fn ago(now: Timestamp, then: Timestamp) -> String {
    let minutes = now.0.saturating_sub(then.0) / 60_000;
    match minutes {
        0 => "just now".to_string(),
        1..60 => format!("{minutes} min ago"),
        60..1440 => format!("{} h ago", minutes / 60),
        1440..2880 => "yesterday".to_string(),
        _ => format!("{} days ago", minutes / 1440),
    }
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::{AccountRef, WorkspaceInfo};

    use super::*;
    use crate::app::tests::key;

    fn here() -> PathBuf {
        std::env::temp_dir().join("project")
    }

    fn info(id: &str, root: PathBuf, first: &str, at: u64) -> SessionInfo {
        SessionInfo {
            id: SessionId::new(id),
            workspace: WorkspaceInfo { name: root.file_name().unwrap().to_string_lossy().into_owned(), root },
            account: AccountRef::new("anthropic", "default"),
            started_at: Timestamp(at),
            first_message: Some(first.to_string()),
        }
    }

    /// Newest first, as the store lists them.
    fn listed() -> SessionList {
        let elsewhere = std::env::temp_dir().join("other");
        let mut list = SessionList::new(&here(), Some(SessionId::new("b")));
        list.listed(Ok(vec![
            info("d", elsewhere.clone(), "deploy the site", 40),
            info("c", here(), "fix the build", 30),
            info("b", here(), "add a test", 20),
            info("a", elsewhere, "write docs", 10),
        ]));
        list
    }

    fn ids(list: &SessionList) -> Vec<&str> {
        list.rows().iter().map(|r| r.info.id.as_str()).collect()
    }

    fn typed(list: &mut SessionList, text: &str) -> ListKey {
        let mut last = ListKey::Nothing;
        for c in text.chars() {
            last = list.key(key(KeyCode::Char(c)));
        }
        last
    }

    #[test]
    fn this_directory_comes_first_and_each_group_is_newest_first() {
        let list = listed();
        assert_eq!(ids(&list), vec!["c", "b", "d", "a"]);
        let rows = list.rows();
        assert!(rows[0].here && rows[1].here && !rows[2].here);
        assert!(rows[1].open && !rows[0].open, "the open session is marked");
    }

    #[test]
    fn typing_filters_by_text_and_searches_transcripts() {
        let mut list = listed();
        assert_eq!(typed(&mut list, "Docs"), ListKey::Search("Docs".into()));
        assert_eq!(ids(&list), vec!["a"], "matches the start message, any case");

        list.key(key(KeyCode::Backspace));
        list.key(key(KeyCode::Backspace));
        list.key(key(KeyCode::Backspace));
        assert_eq!(list.key(key(KeyCode::Backspace)), ListKey::Nothing, "blank: nothing to search");
        assert_eq!(ids(&list).len(), 4);

        typed(&mut list, "needle");
        assert!(list.rows().is_empty());
        let hit = |id: &str| SearchHit { session: SessionId::new(id), snippet: format!("…the needle in {id}…") };
        list.found("needl", Ok(vec![hit("a")]));
        assert!(list.rows().is_empty(), "an answer for an older query is dropped");
        list.found("needle", Ok(vec![hit("d"), hit("b")]));
        let rows = list.rows();
        assert_eq!(ids(&list), vec!["b", "d"], "grouping still applies");
        assert_eq!(rows[0].snippet, Some("…the needle in b…"));

        list.found("needle", Err("the database is locked".into()));
        assert_eq!(list.search_error.as_deref(), Some("the database is locked"));
    }

    #[test]
    fn enter_resumes_the_selected_row_and_esc_closes() {
        let mut list = listed();
        assert_eq!(list.key(key(KeyCode::Enter)), ListKey::Resume(SessionId::new("c")));
        list.key(key(KeyCode::Down));
        assert_eq!(list.key(key(KeyCode::Enter)), ListKey::AlreadyOpen);
        for _ in 0..10 {
            list.key(key(KeyCode::Down));
        }
        assert_eq!(list.key(key(KeyCode::Enter)), ListKey::Resume(SessionId::new("a")), "stops at the last row");
        list.key(key(KeyCode::Up));
        assert_eq!(list.key(key(KeyCode::Enter)), ListKey::Resume(SessionId::new("d")));
        assert_eq!(list.key(key(KeyCode::Esc)), ListKey::Close);

        let mut empty = SessionList::new(&here(), None);
        assert!(empty.loading());
        assert_eq!(empty.key(key(KeyCode::Enter)), ListKey::Nothing);
        empty.listed(Err("can't read".into()));
        assert_eq!(empty.list_error(), Some("can't read"));
    }

    #[test]
    fn times_are_relative() {
        let min = 60_000;
        assert_eq!(ago(Timestamp(30_000), Timestamp(0)), "just now");
        assert_eq!(ago(Timestamp(5 * min), Timestamp(0)), "5 min ago");
        assert_eq!(ago(Timestamp(3 * 60 * min), Timestamp(0)), "3 h ago");
        assert_eq!(ago(Timestamp(30 * 60 * min), Timestamp(0)), "yesterday");
        assert_eq!(ago(Timestamp(5 * 1440 * min), Timestamp(0)), "5 days ago");
        assert_eq!(ago(Timestamp(0), Timestamp(10)), "just now", "a clock that went back");
    }
}
