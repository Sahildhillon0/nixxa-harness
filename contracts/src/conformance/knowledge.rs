//! Conformance suite for [`Knowledge`].

use std::collections::HashSet;

use super::{id, workspace};
use crate::{Knowledge, KnowledgeEntry, KnowledgeError, SourceId, SourceStatus};

/// Runs every scenario on `knowledge`. Knowledge sources are external, so the suite only
/// reads, except for one `record` against an unknown source (which must fail).
pub async fn run(knowledge: &dyn Knowledge) {
    let sources = knowledge.sources().await;
    let ids: HashSet<_> = sources.iter().map(|s| s.id.clone()).collect();
    assert_eq!(ids.len(), sources.len(), "source ids must be unique");
    let ready: HashSet<_> = sources.iter().filter(|s| s.status == SourceStatus::Ready).map(|s| s.id.clone()).collect();

    let tools = knowledge.tools().await;
    for t in &tools {
        assert!(ready.contains(&t.source), "tool `{}` belongs to non-ready or unknown source {}", t.name, t.source);
        assert!(!t.name.is_empty() && !t.description.is_empty(), "tools need a name and description");
        assert!(t.input_schema.is_object(), "tool `{}` input_schema must be a JSON object", t.name);
    }

    let primer = knowledge.primer(&workspace()).await.expect("primer");
    for s in &primer.sections {
        assert!(ids.contains(&s.source), "primer section from unknown source {}", s.source);
    }

    let ghost = SourceId::new("conformance-no-such-source");
    let r = knowledge.call(id(1), &ghost, "anything", serde_json::json!({})).await;
    assert!(matches!(r, Err(KnowledgeError::SourceUnavailable(_))), "unknown source: got {r:?}");
    let entry = KnowledgeEntry { title: "t".into(), body: "b".into(), tags: vec![] };
    let r = knowledge.record(id(2), &ghost, entry).await;
    assert!(matches!(r, Err(KnowledgeError::SourceUnavailable(_))), "record to unknown source: got {r:?}");

    for source in sources.iter().filter(|s| s.status != SourceStatus::Ready) {
        let r = knowledge.call(id(3), &source.id, "anything", serde_json::json!({})).await;
        assert!(matches!(r, Err(KnowledgeError::SourceUnavailable(_))), "non-ready source {}: got {r:?}", source.id);
    }

    if let Some(source) = ready.iter().next() {
        let r = knowledge.call(id(4), source, "conformance-no-such-tool", serde_json::json!({})).await;
        assert!(matches!(r, Err(KnowledgeError::UnknownTool { .. })), "unknown tool on {source}: got {r:?}");
    }
}
