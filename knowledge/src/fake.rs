//! Fakes for the knowledge contract.

use async_trait::async_trait;

use nixxa_contracts::{
    Knowledge, KnowledgeEntry, KnowledgeError, Observation, Outcome, Primer, PrimerSection, SourceId, SourceInfo,
    SourceStatus, ToolSpec, WorkspaceInfo,
};

/// A fake knowledge source set: `notes` is ready with one non-mutating `search` tool, `vault`
/// is missing.
pub struct FakeKnowledge;

#[async_trait]
impl Knowledge for FakeKnowledge {
    async fn sources(&self) -> Vec<SourceInfo> {
        vec![
            SourceInfo { id: SourceId::new("notes"), name: "Notes".into(), status: SourceStatus::Ready },
            SourceInfo { id: SourceId::new("vault"), name: "Vault".into(), status: SourceStatus::Missing },
        ]
    }

    async fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            source: SourceId::new("notes"),
            name: "search".into(),
            description: "Search the user's notes.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "query": { "type": "string" } } }),
            mutating: false,
        }]
    }

    async fn primer(&self, _workspace: &WorkspaceInfo) -> Result<Primer, KnowledgeError> {
        Ok(Primer {
            sections: vec![PrimerSection {
                source: SourceId::new("notes"),
                title: "Recent memory".into(),
                body: "canned primer body".into(),
            }],
        })
    }

    async fn call(
        &self,
        id: nixxa_contracts::ActionId,
        source: &SourceId,
        tool: &str,
        _args: serde_json::Value,
    ) -> Result<Observation, KnowledgeError> {
        if source.as_str() != "notes" {
            return Err(KnowledgeError::SourceUnavailable(source.clone()));
        }
        if tool != "search" {
            return Err(KnowledgeError::UnknownTool { source_id: source.clone(), tool: tool.into() });
        }
        Ok(Observation::new(id, Outcome::Text { content: "canned search result".into() }))
    }

    async fn record(
        &self,
        id: nixxa_contracts::ActionId,
        source: &SourceId,
        _entry: KnowledgeEntry,
    ) -> Result<Observation, KnowledgeError> {
        if source.as_str() != "notes" {
            return Err(KnowledgeError::SourceUnavailable(source.clone()));
        }
        Ok(Observation::new(id, Outcome::Recorded { location: None }))
    }
}

#[cfg(all(test, feature = "conformance"))]
mod tests {
    use nixxa_contracts::conformance::knowledge::run;

    use super::FakeKnowledge;

    #[tokio::test]
    async fn fake_knowledge_conforms() {
        run(&FakeKnowledge).await;
    }
}
