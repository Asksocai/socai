//! Offline fault injection of the actual agent loop, not business acceptance.
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use socai_core::agent::{self, Backend, Block, LLMResponse, Message, StopReason, TokenUsage, ToolCall, ToolSchema};
use std::{path::PathBuf, sync::{Arc, Mutex}};

#[derive(Clone)]
struct DiagnosticBackend {
    mode: &'static str,
    disabled: bool,
    calls: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Backend for DiagnosticBackend {
    fn label(&self) -> String { "offline reasoning recovery".into() }
    fn provider(&self) -> &str { "diagnostic" }
    fn model(&self) -> &str { "same-model" }
    fn recover_reasoning_exhaustion(&self) -> Option<Arc<dyn Backend>> {
        if self.disabled || self.mode == "unsupported" { return None; }
        Some(Arc::new(Self { disabled: true, ..self.clone() }))
    }
    async fn send(&self, _: &str, messages: &[Message], tools: &[ToolSchema], _: u32) -> Result<LLMResponse> {
        let mut calls = self.calls.lock().unwrap();
        let index = calls.len();
        ensure!(self.mode == "tool-at-limit" || !messages.iter().flat_map(|m| m.content.as_blocks()).any(|b|
            matches!(b, Block::ReasoningContent { text } if text == "discarded incomplete reasoning")),
            "an incomplete response was replayed");
        if self.mode == "tool-at-limit" && index > 0 {
            ensure!(messages.iter().flat_map(|m| m.content.as_blocks()).any(|b|
                matches!(b, Block::ReasoningContent { text } if text == "discarded incomplete reasoning")),
                "reasoning attached to an executed tool call was lost");
        }
        let has_result = messages.iter().flat_map(|m| m.content.as_blocks()).any(|b|
            matches!(b, Block::ToolResult { tool_use_id, .. } if tool_use_id == "one-action"));
        calls.push(json!({"index":index,"disabled":self.disabled,"tools":tools.iter().map(|t|&t.name).collect::<Vec<_>>(),"has_result":has_result}));
        let mut response = LLMResponse {
            text_blocks: vec![], tool_calls: vec![], stop_reason: StopReason::EndTurn,
            usage: TokenUsage { input_tokens: 10, output_tokens: 7, ..Default::default() },
            provider_usage: None, stream_timing: None, reasoning_content: String::new(), thinking_blocks: vec![], reasoning_items: vec![],
        };
        if self.mode == "never-recovers" || (self.mode == "unsupported" && index < 3)
            || (index == 0 && self.mode != "normal") {
            response.stop_reason = StopReason::MaxTokens;
            if self.mode == "visible-truncation" { response.text_blocks.push("incomplete answer".into()); }
            else {
                response.reasoning_content = "discarded incomplete reasoning".into();
                response.usage.reasoning_output_tokens = Some(7);
            }
            // A complete tool call must still execute, even if the text budget ends.
            if self.mode == "tool-at-limit" {
                response.tool_calls.push(ToolCall { id: "one-action".into(), name: "echo".into(), input: json!({"text":"collected fixture"}) });
            }
        } else if self.mode == "recover-to-tool" && !has_result {
            ensure!(self.disabled, "same reasoning request retried without recovery");
            response.tool_calls.push(ToolCall { id: "one-action".into(), name: "echo".into(), input: json!({"text":"collected fixture"}) });
            response.stop_reason = StopReason::ToolUse;
        } else {
            if matches!(self.mode, "recover-to-tool" | "summary") {
                ensure!(self.disabled, "reasoning exhaustion did not switch the next request");
            }
            response.text_blocks.push("Complete fixture answer.".into());
        }
        Ok(response)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).context("new diagnostic evidence directory required")?);
    ensure!(!dir.exists(), "never overwrite diagnostic evidence");
    std::fs::create_dir(&dir)?;
    std::env::set_var("SOCAI_HOME", dir.join("home"));
    std::env::set_var("SOCAI_TELEMETRY", "off");
    let mut results = vec![];
    for mode in ["recover-to-tool", "summary", "normal", "visible-truncation", "tool-at-limit", "unsupported", "never-recovers"] {
        let calls = Arc::new(Mutex::new(vec![]));
        let backend = Arc::new(DiagnosticBackend { mode, disabled: false, calls: calls.clone() });
        let outcome = agent::run_agent("Offline fault-injection case.", backend.clone(), vec![Arc::new(agent::EchoTool)], agent::AgentOptions {
            run_dir: Some(dir.join(mode)), local_memory: false, max_steps: if mode == "summary" { 0 } else { 8 },
            max_tokens: 7, ..Default::default()
        }).await?;
        let calls = calls.lock().unwrap();
        let recovered = calls.iter().filter(|c|c["disabled"] == true).count();
        let text = &outcome.final_text;
        // Persist the actual calls/outcome before assertions, including failures.
        std::fs::write(dir.join(format!("{mode}-observed.json")), serde_json::to_vec_pretty(&json!({
            "mode":mode,"calls":*calls,"answer":text,"error":outcome.error,"usage":outcome.usage,
            "degraded_reason":outcome.degraded_reason,
        }))?)?;
        ensure!(!backend.disabled, "the original backend was mutated");
        ensure!(outcome.usage.input_tokens == calls.len() as u64 * 10
            && outcome.usage.output_tokens == calls.len() as u64 * 7,
            "a discarded response disappeared from usage");
        if mode == "never-recovers" {
            ensure!(outcome.error.is_some() && calls.len() == 5, "bounded failure policy changed");
        } else {
            ensure!(outcome.error.is_none() && text == "Complete fixture answer.", "complete answer missing");
        }
        match mode {
            "recover-to-tool" => {
                ensure!(calls.len() == 3 && recovered == 2 && calls[1]["tools"] == json!(["echo"]) && calls[2]["has_result"] == true,
                    "recovery lost tools, repeated an action or failed to retain recovery");
            }
            "summary" => ensure!(calls.len() == 2 && recovered == 1 && calls[1]["tools"] == json!([]), "summary recovery changed tool access"),
            "normal" => ensure!(calls.len() == 1 && recovered == 0, "ordinary request changed"),
            "visible-truncation" | "tool-at-limit" => ensure!(calls.len() == 2 && recovered == 0, "non-reasoning output changed mode"),
            "unsupported" => ensure!(calls.len() == 4 && recovered == 0, "unsupported backend changed mode"),
            _ => {}
        }
        results.push(json!({"mode":mode,"requests":calls.len(),"recovered_requests":recovered,"original_unchanged":true,
            "complete":outcome.error.is_none(),"partial":outcome.degraded_reason.is_some(),"calls":*calls,"usage":outcome.usage}));
    }
    let result = json!({"phase":"passed-offline-fault-injection","network_used":false,"browser_used":false,"business_acceptance":false,"cases":results});
    std::fs::write(dir.join("result.json"), serde_json::to_vec_pretty(&result)?)?;
    println!("{result}");
    Ok(())
}
