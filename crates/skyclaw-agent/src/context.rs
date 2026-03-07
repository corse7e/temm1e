//! Context builder — assembles a CompletionRequest from session history,
//! memory search results, system prompt, and tool definitions.

use std::sync::Arc;

use skyclaw_core::Memory;
use skyclaw_core::SearchOpts;
use skyclaw_core::Tool;
use skyclaw_core::types::message::{
    ChatMessage, CompletionRequest, MessageContent, Role, ToolDefinition,
};
use skyclaw_core::types::session::SessionContext;

/// Build a CompletionRequest from all available context.
pub async fn build_context(
    session: &SessionContext,
    memory: &dyn Memory,
    tools: &[Arc<dyn Tool>],
    model: &str,
    system_prompt: Option<&str>,
) -> CompletionRequest {
    let mut messages: Vec<ChatMessage> = Vec::new();

    // 1. Retrieve relevant memory entries for context augmentation
    let query = session
        .history
        .last()
        .and_then(|m| match &m.content {
            MessageContent::Text(t) => Some(t.clone()),
            MessageContent::Parts(parts) => parts.iter().find_map(|p| match p {
                skyclaw_core::types::message::ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            }),
        })
        .unwrap_or_default();

    if !query.is_empty() {
        let opts = SearchOpts {
            limit: 5,
            session_filter: Some(session.session_id.clone()),
            ..Default::default()
        };

        if let Ok(entries) = memory.search(&query, opts).await {
            if !entries.is_empty() {
                let memory_text: String = entries
                    .iter()
                    .map(|e| format!("[{}] {}", e.timestamp.format("%Y-%m-%d %H:%M"), e.content))
                    .collect::<Vec<_>>()
                    .join("\n");

                messages.push(ChatMessage {
                    role: Role::System,
                    content: MessageContent::Text(format!(
                        "Relevant context from memory:\n{}",
                        memory_text
                    )),
                });
            }
        }
    }

    // 2. Append session conversation history
    messages.extend(session.history.clone());

    // 3. Build tool definitions
    let tool_defs: Vec<ToolDefinition> = tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name().to_string(),
            description: t.description().to_string(),
            parameters: t.parameters_schema(),
        })
        .collect();

    // 4. Assemble the system prompt
    let system = system_prompt.map(|s| s.to_string()).or_else(|| {
        Some(
            "You are SkyClaw, a cloud-native AI agent. You have access to tools for \
             shell execution, file operations, browsing, and more. Use them when needed \
             to assist the user. Always be precise and security-conscious."
                .to_string(),
        )
    });

    CompletionRequest {
        model: model.to_string(),
        messages,
        tools: tool_defs,
        max_tokens: Some(4096),
        temperature: Some(0.7),
        system,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyclaw_test_utils::{MockMemory, MockTool, make_session};

    #[tokio::test]
    async fn context_includes_system_prompt() {
        let memory = MockMemory::new();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let session = make_session();

        let req = build_context(&session, &memory, &tools, "test-model", Some("Custom prompt")).await;
        assert_eq!(req.system.as_deref(), Some("Custom prompt"));
        assert_eq!(req.model, "test-model");
    }

    #[tokio::test]
    async fn context_default_system_prompt() {
        let memory = MockMemory::new();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let session = make_session();

        let req = build_context(&session, &memory, &tools, "test-model", None).await;
        assert!(req.system.is_some());
        assert!(req.system.unwrap().contains("SkyClaw"));
    }

    #[tokio::test]
    async fn context_includes_tool_definitions() {
        let memory = MockMemory::new();
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool::new("shell")),
            Arc::new(MockTool::new("browser")),
        ];
        let session = make_session();

        let req = build_context(&session, &memory, &tools, "model", None).await;
        assert_eq!(req.tools.len(), 2);
        assert_eq!(req.tools[0].name, "shell");
        assert_eq!(req.tools[1].name, "browser");
    }

    #[tokio::test]
    async fn context_includes_conversation_history() {
        let memory = MockMemory::new();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let mut session = make_session();
        session.history.push(ChatMessage {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        });
        session.history.push(ChatMessage {
            role: Role::Assistant,
            content: MessageContent::Text("Hi there".to_string()),
        });

        let req = build_context(&session, &memory, &tools, "model", None).await;
        // Messages should include the history
        assert!(req.messages.len() >= 2);
    }
}
