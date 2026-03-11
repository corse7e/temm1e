//! CodexResponsesProvider — Provider trait implementation using the OpenAI Responses API.
//!
//! This is NOT a modification of `OpenAICompatProvider`. The Responses API has a
//! fundamentally different request/response shape than Chat Completions:
//! - `input` + `instructions` instead of `messages`
//! - `output` items instead of `choices[0].message`
//! - Different tool call schema (function_call / function_call_output)
//! - Different streaming event types

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use skyclaw_core::types::error::SkyclawError;
use skyclaw_core::types::message::*;
use skyclaw_core::Provider;
use std::sync::Arc;

use crate::token_store::TokenStore;

/// Provider that uses OpenAI Responses API with OAuth tokens.
pub struct CodexResponsesProvider {
    token_store: Arc<TokenStore>,
    #[allow(dead_code)]
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl CodexResponsesProvider {
    /// Create a new Codex Responses API provider.
    pub fn new(model: String, token_store: Arc<TokenStore>) -> Self {
        Self {
            token_store,
            model,
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Build the Responses API request body from a CompletionRequest.
    fn build_request_body(
        &self,
        request: &CompletionRequest,
        stream: bool,
    ) -> Result<serde_json::Value, SkyclawError> {
        // Extract system message → "instructions" field
        let instructions = request
            .system
            .clone()
            .or_else(|| {
                request.messages.iter().find_map(|m| {
                    if matches!(m.role, Role::System) {
                        Some(m.content.as_text().to_string())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_default();

        // Convert messages → "input" items (skip system messages)
        let input: Vec<serde_json::Value> = request
            .messages
            .iter()
            .filter(|m| !matches!(m.role, Role::System))
            .flat_map(|m| self.convert_message(m))
            .collect();

        let mut body = serde_json::json!({
            "model": request.model,
            "input": input,
            "stream": stream,
            "store": false,
        });

        if !instructions.is_empty() {
            body["instructions"] = serde_json::Value::String(instructions);
        }

        // Note: Codex backend does not support max_output_tokens or temperature

        // Convert tools
        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
        }

        Ok(body)
    }

    /// Convert a ChatMessage to one or more Responses API input items.
    fn convert_message(&self, msg: &ChatMessage) -> Vec<serde_json::Value> {
        match &msg.content {
            MessageContent::Text(text) => {
                match msg.role {
                    Role::User => vec![serde_json::json!({
                        "role": "user",
                        "content": text,
                    })],
                    Role::Assistant => vec![serde_json::json!({
                        "role": "assistant",
                        "content": text,
                    })],
                    Role::Tool => {
                        // Tool results — should not appear as Text in practice,
                        // but handle gracefully
                        vec![]
                    }
                    Role::System => vec![], // Filtered out above
                }
            }
            MessageContent::Parts(parts) => {
                let mut items = Vec::new();
                for part in parts {
                    match part {
                        ContentPart::Text { text } => {
                            items.push(serde_json::json!({
                                "role": match msg.role {
                                    Role::User => "user",
                                    Role::Assistant => "assistant",
                                    _ => "user",
                                },
                                "content": text,
                            }));
                        }
                        ContentPart::ToolUse { id, name, input } => {
                            // Assistant requested a tool call → function_call item
                            // Skip if name is empty (malformed history entry)
                            if !name.is_empty() {
                                items.push(serde_json::json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": input.to_string(),
                                }));
                            }
                        }
                        ContentPart::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            // Tool result → function_call_output item
                            items.push(serde_json::json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": content,
                            }));
                        }
                        ContentPart::Image { media_type, data } => {
                            items.push(serde_json::json!({
                                "role": "user",
                                "content": [{
                                    "type": "input_image",
                                    "image_url": format!("data:{};base64,{}", media_type, data),
                                }],
                            }));
                        }
                    }
                }
                items
            }
        }
    }
}

#[async_trait]
impl Provider for CodexResponsesProvider {
    fn name(&self) -> &str {
        "openai-codex"
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, SkyclawError> {
        // Codex backend requires streaming — collect stream into a single response
        tracing::debug!(model = %request.model, "Codex Responses API request (stream-collected)");

        let mut stream = self.stream(request).await?;

        let mut full_text = String::new();
        let mut tool_uses = Vec::new();
        let mut stop_reason = None;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            if let Some(delta) = chunk.delta {
                full_text.push_str(&delta);
            }
            if let Some(tool_use) = chunk.tool_use {
                tool_uses.push(tool_use);
            }
            if chunk.stop_reason.is_some() {
                stop_reason = chunk.stop_reason;
            }
        }

        let mut content = Vec::new();
        if !full_text.is_empty() {
            content.push(ContentPart::Text {
                text: full_text.clone(),
            });
        }
        content.extend(tool_uses);

        Ok(CompletionResponse {
            id: String::new(),
            content,
            stop_reason,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
            },
        })
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'_, Result<StreamChunk, SkyclawError>>, SkyclawError> {
        let token = self.token_store.get_access_token().await?;
        let body = self.build_request_body(&request, true)?;

        tracing::debug!(model = %request.model, "Codex Responses API request (streaming)");

        let resp = self
            .client
            .post(format!("{}/responses", self.base_url))
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| SkyclawError::Provider(format!("Responses API request failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(SkyclawError::Auth(format!(
                    "Codex OAuth token rejected ({}): {}",
                    status, body_text
                )));
            }
            return Err(SkyclawError::Provider(format!(
                "Responses API error ({}): {}",
                status, body_text
            )));
        }

        let byte_stream = resp.bytes_stream();

        // State for the streaming parser:
        // (byte_stream, buffer, accumulated_tool_calls)
        type ToolCallAcc = (String, String, String); // (call_id, name, arguments)
        let stream = futures::stream::unfold(
            (
                Box::pin(byte_stream),
                String::new(),
                Vec::<ToolCallAcc>::new(),
            ),
            |(mut byte_stream, mut buffer, mut tool_calls)| async move {
                loop {
                    // Try to extract complete SSE events from buffer
                    while let Some(event_end) = buffer.find("\n\n") {
                        let event_block = buffer[..event_end].to_string();
                        buffer = buffer[event_end + 2..].to_string();

                        // Parse SSE event
                        let mut event_type = String::new();
                        let mut data_line = String::new();
                        for line in event_block.lines() {
                            if let Some(et) = line.strip_prefix("event: ") {
                                event_type = et.to_string();
                            } else if let Some(d) = line.strip_prefix("data: ") {
                                data_line = d.to_string();
                            }
                        }

                        if data_line.is_empty() || data_line == "[DONE]" {
                            // Flush accumulated tool calls one at a time
                            if let Some((call_id, name, args)) = tool_calls.pop() {
                                let input: serde_json::Value = serde_json::from_str(&args)
                                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                                return Some((
                                    Ok(StreamChunk {
                                        delta: None,
                                        tool_use: Some(ContentPart::ToolUse {
                                            id: call_id,
                                            name,
                                            input,
                                        }),
                                        stop_reason: None,
                                    }),
                                    (byte_stream, buffer, tool_calls),
                                ));
                            }
                            if data_line == "[DONE]" {
                                return Some((
                                    Ok(StreamChunk {
                                        delta: None,
                                        tool_use: None,
                                        stop_reason: Some("end_turn".to_string()),
                                    }),
                                    (byte_stream, buffer, tool_calls),
                                ));
                            }
                            continue;
                        }

                        // Parse the JSON data
                        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&data_line);
                        let data = match parsed {
                            Ok(d) => d,
                            Err(_) => continue,
                        };

                        match event_type.as_str() {
                            "response.output_text.delta" => {
                                if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                                    return Some((
                                        Ok(StreamChunk {
                                            delta: Some(delta.to_string()),
                                            tool_use: None,
                                            stop_reason: None,
                                        }),
                                        (byte_stream, buffer, tool_calls),
                                    ));
                                }
                            }
                            "response.function_call_arguments.delta" => {
                                let call_id = data
                                    .get("call_id")
                                    .or_else(|| data.get("item_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = data
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let delta =
                                    data.get("delta").and_then(|v| v.as_str()).unwrap_or("");

                                if let Some(existing) =
                                    tool_calls.iter_mut().find(|tc| tc.0 == call_id)
                                {
                                    existing.2.push_str(delta);
                                } else if !call_id.is_empty() {
                                    tool_calls.push((call_id, name, delta.to_string()));
                                }
                            }
                            "response.output_item.done" => {
                                // Check if this is a function_call item completing
                                if let Some(item) = data.get("item") {
                                    if item.get("type").and_then(|t| t.as_str())
                                        == Some("function_call")
                                    {
                                        let call_id = item
                                            .get("call_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let name = item
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let args = item
                                            .get("arguments")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("{}")
                                            .to_string();

                                        // Remove from accumulator if present
                                        tool_calls.retain(|tc| tc.0 != call_id);

                                        let input: serde_json::Value = serde_json::from_str(&args)
                                            .unwrap_or(serde_json::Value::Object(
                                                serde_json::Map::new(),
                                            ));
                                        return Some((
                                            Ok(StreamChunk {
                                                delta: None,
                                                tool_use: Some(ContentPart::ToolUse {
                                                    id: call_id,
                                                    name,
                                                    input,
                                                }),
                                                stop_reason: None,
                                            }),
                                            (byte_stream, buffer, tool_calls),
                                        ));
                                    }
                                }
                            }
                            "response.completed" => {
                                // Final event — flush remaining tool calls one at a time
                                if let Some((call_id, name, args)) = tool_calls.pop() {
                                    let input: serde_json::Value = serde_json::from_str(&args)
                                        .unwrap_or(serde_json::Value::Object(
                                            serde_json::Map::new(),
                                        ));
                                    return Some((
                                        Ok(StreamChunk {
                                            delta: None,
                                            tool_use: Some(ContentPart::ToolUse {
                                                id: call_id,
                                                name,
                                                input,
                                            }),
                                            stop_reason: None,
                                        }),
                                        (byte_stream, buffer, tool_calls),
                                    ));
                                }

                                let stop = data
                                    .get("response")
                                    .and_then(|r| r.get("status"))
                                    .and_then(|s| s.as_str())
                                    .map(|s| {
                                        if s == "completed" {
                                            "end_turn".to_string()
                                        } else {
                                            s.to_string()
                                        }
                                    })
                                    .unwrap_or_else(|| "end_turn".to_string());

                                return Some((
                                    Ok(StreamChunk {
                                        delta: None,
                                        tool_use: None,
                                        stop_reason: Some(stop),
                                    }),
                                    (byte_stream, buffer, tool_calls),
                                ));
                            }
                            _ => {
                                // Ignore other event types (response.created, etc.)
                            }
                        }
                    }

                    // Need more data from the stream
                    match byte_stream.next().await {
                        Some(Ok(bytes)) => {
                            buffer.push_str(&String::from_utf8_lossy(&bytes));
                        }
                        Some(Err(e)) => {
                            return Some((
                                Err(SkyclawError::Provider(format!("Stream error: {}", e))),
                                (byte_stream, buffer, tool_calls),
                            ));
                        }
                        None => {
                            // Stream ended — flush remaining tool calls one at a time
                            if let Some((call_id, name, args)) = tool_calls.pop() {
                                let input: serde_json::Value = serde_json::from_str(&args)
                                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                                return Some((
                                    Ok(StreamChunk {
                                        delta: None,
                                        tool_use: Some(ContentPart::ToolUse {
                                            id: call_id,
                                            name,
                                            input,
                                        }),
                                        stop_reason: None,
                                    }),
                                    (byte_stream, buffer, tool_calls),
                                ));
                            }
                            return None;
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }

    async fn health_check(&self) -> Result<bool, SkyclawError> {
        // Try to get a fresh token — if this works, the OAuth connection is healthy
        match self.token_store.get_access_token().await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    async fn list_models(&self) -> Result<Vec<String>, SkyclawError> {
        // Return the known Codex-compatible models
        Ok(vec![
            "gpt-5.4".to_string(),
            "gpt-5.3-codex".to_string(),
            "gpt-5.3-codex-spark".to_string(),
            "gpt-5.2-codex".to_string(),
            "gpt-5-codex".to_string(),
            "gpt-5-codex-mini".to_string(),
            "gpt-5-mini".to_string(),
        ])
    }
}

// ── Helper trait for MessageContent ──────────────────────────

trait MessageContentExt {
    fn as_text(&self) -> &str;
}

impl MessageContentExt for MessageContent {
    fn as_text(&self) -> &str {
        match self {
            MessageContent::Text(t) => t.as_str(),
            MessageContent::Parts(parts) => {
                for p in parts {
                    if let ContentPart::Text { text } = p {
                        return text.as_str();
                    }
                }
                ""
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_sse_data() {
        let data = r#"{"delta":"Hello"}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["delta"], "Hello");
    }

    #[test]
    fn parse_tool_call_sse_data() {
        let data = r#"{"call_id":"call_123","name":"shell","delta":"{\"command\": \"ls\"}"}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["call_id"], "call_123");
        assert_eq!(parsed["name"], "shell");
    }

    #[test]
    fn build_request_extracts_system() {
        let store = Arc::new(TokenStore::new(crate::token_store::CodexOAuthTokens {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: u64::MAX,
            email: "test@test.com".into(),
            account_id: "org-test".into(),
        }));
        let provider = CodexResponsesProvider::new("gpt-5.3-codex".into(), store);

        let request = CompletionRequest {
            model: "gpt-5.3-codex".into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text("Hello".into()),
            }],
            tools: vec![],
            max_tokens: Some(1024),
            temperature: Some(0.7),
            system: Some("You are a helpful assistant.".into()),
        };

        let body = provider.build_request_body(&request, false).unwrap();
        assert_eq!(body["instructions"], "You are a helpful assistant.");
        assert_eq!(body["model"], "gpt-5.3-codex");
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn build_request_converts_tools() {
        let store = Arc::new(TokenStore::new(crate::token_store::CodexOAuthTokens {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: u64::MAX,
            email: "test@test.com".into(),
            account_id: "org-test".into(),
        }));
        let provider = CodexResponsesProvider::new("gpt-5.3-codex".into(), store);

        let request = CompletionRequest {
            model: "gpt-5.3-codex".into(),
            messages: vec![],
            tools: vec![ToolDefinition {
                name: "shell".into(),
                description: "Run a shell command".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    }
                }),
            }],
            max_tokens: None,
            temperature: None,
            system: None,
        };

        let body = provider.build_request_body(&request, false).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "shell");
    }

    #[test]
    fn convert_tool_result_message() {
        let store = Arc::new(TokenStore::new(crate::token_store::CodexOAuthTokens {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: u64::MAX,
            email: "test@test.com".into(),
            account_id: "org-test".into(),
        }));
        let provider = CodexResponsesProvider::new("gpt-5.3-codex".into(), store);

        let msg = ChatMessage {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: "call_123".into(),
                content: "file.txt found".into(),
                is_error: false,
            }]),
        };

        let items = provider.convert_message(&msg);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "call_123");
        assert_eq!(items[0]["output"], "file.txt found");
    }
}
