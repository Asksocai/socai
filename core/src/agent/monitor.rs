//! Recurring reports may cite only the host's completed, current comparison.
//! This is a delivery guard, not a second search or another model call.
use std::collections::BTreeSet;

use serde_json::Value;

use super::extensions::{AgentExtension, FinishDecision, ToolFeedback};
use super::tool::{ToolContext, ToolResult};

const COMPARISON_REQUIRED: &str = "The draft monitoring report was withheld: no complete comparison of the current saved evidence exists. Call read_research_history with compare_current:true (start at offset 0), follow every next_request, then answer using only its qualified new/changed items. Do not repeat collection or treat an unavailable comparison as zero changes.";

#[derive(Default)]
pub(crate) struct MonitorExtension {
    comparison: Option<Comparison>,
    comparison_repair_attempted: bool,
}

struct Comparison {
    id: String,
    fingerprint: String,
    total: u64,
    received: u64,
    next_offset: Option<u64>,
    sources: BTreeSet<String>,
    report_limit: Option<u64>,
}

impl MonitorExtension {
    fn completed_comparison(&self, ctx: &ToolContext) -> Option<&Comparison> {
        self.comparison.as_ref().filter(|comparison| {
            comparison.next_offset.is_none() && comparison.received == comparison.total
                && ctx.notes_fingerprint().as_deref() == Some(comparison.fingerprint.as_str())
        })
    }

    fn observe(&mut self, ctx: &ToolContext, input: &Value, result: &ToolResult) -> Option<()> {
        let offset = input.get("offset").map_or(Some(0), Value::as_u64)?;
        let body: Value = serde_json::from_str(&result.flat_text()).ok()?;
        if body["available"] != true || body["coverage"]["current_complete"] != true {
            return None;
        }
        let id = body["comparison_id"].as_str()?;
        if id.len() != 64
            || !id.bytes().all(|b| b.is_ascii_hexdigit())
            || body["offset"].as_u64()? != offset
        {
            return None;
        }
        let total = body["total_items"].as_u64()?;
        let report_limit = if body["report_limit"].is_null() {
            None // Compatible with hosts predating the optional post cap.
        } else {
            Some(body["report_limit"].as_u64().filter(|limit| *limit > 0)?)
        };
        let items = body["items"].as_array()?;
        let fingerprint = ctx.notes_fingerprint()?;
        if offset == 0 {
            self.comparison = Some(Comparison {
                id: id.into(),
                fingerprint: fingerprint.clone(),
                total,
                received: 0,
                next_offset: Some(0),
                sources: BTreeSet::new(),
                report_limit,
            });
        }
        let comparison = self.comparison.as_mut()?;
        if comparison.id != id
            || comparison.fingerprint != fingerprint
            || comparison.total != total
            || comparison.report_limit != report_limit
            || comparison.next_offset != Some(offset)
        {
            return None;
        }
        let received = offset.checked_add(items.len() as u64)?;
        if received > total {
            return None;
        }
        let next_offset = if body["next_request"].is_null() {
            if received != total {
                return None;
            }
            None
        } else {
            let next = &body["next_request"];
            if next["compare_current"] != true
                || next["offset"].as_u64()? != received
                || received >= total
                || items.is_empty()
            {
                return None;
            }
            Some(received)
        };
        for item in items {
            if matches!(item["change"].as_str(), Some("new" | "changed"))
                && item["id"].as_str().is_some_and(|id| !id.is_empty())
            {
                if let Some(source) = item["url"].as_str().and_then(source_key) {
                    comparison.sources.insert(source);
                }
            }
        }
        comparison.received = received;
        comparison.next_offset = next_offset;
        Some(())
    }
}

impl AgentExtension for MonitorExtension {
    fn after_tool_call(
        &mut self,
        ctx: &ToolContext,
        name: &str,
        input: &Value,
        result: &ToolResult,
        error: Option<&str>,
    ) -> ToolFeedback {
        if name == "read_research_history" && input["compare_current"] == true {
            if error.is_some() || result.failed() || self.observe(ctx, input, result).is_none() {
                self.comparison = None;
            } else {
                self.comparison_repair_attempted = false;
            }
        }
        ToolFeedback::default()
    }

    fn before_finish(&mut self, ctx: &ToolContext, steps_remaining: u32) -> FinishDecision {
        if self.completed_comparison(ctx).is_some() {
            return FinishDecision::Allow;
        }
        if self.comparison_repair_attempted || steps_remaining == 0 {
            return FinishDecision::Partial("Monitoring ended without a complete current comparison.".into());
        }
        // Repair the prerequisite before answer validation. Missing comparison
        // must not spend the one correction reserved for a bad report draft.
        self.comparison_repair_attempted = true;
        FinishDecision::Continue(COMPARISON_REQUIRED.into())
    }

    fn validate_answer(&self, ctx: &ToolContext, answer: &str) -> Result<(), String> {
        let comparison = self.completed_comparison(ctx).ok_or_else(|| COMPARISON_REQUIRED.to_string())?;
        let lower = answer.to_ascii_lowercase();
        let mut cited_sources = BTreeSet::new();
        for (start, _) in lower.match_indices("http") {
            if !lower[start..].starts_with("https://") && !lower[start..].starts_with("http://") {
                continue;
            }
            let url: String = answer[start..]
                .chars()
                .take_while(|c| {
                    !c.is_whitespace()
                        && !matches!(
                            c,
                            '<' | '>'
                                | '"'
                                | '\''
                                | ')'
                                | ']'
                                | '}'
                                | '，'
                                | '。'
                                | '；'
                                | '！'
                                | '？'
                                | '、'
                        )
                })
                .collect();
            let url = url.trim_end_matches(['.', ',', ';', '!', '?', '*', '`']);
            let key = source_key(url).filter(|key| comparison.sources.contains(key));
            if let Some(key) = key {
                cited_sources.insert(key);
            } else {
                return Err(format!("The draft monitoring report was withheld because it cites a source outside this occurrence's qualified new/changed items: {url}. Rewrite using only the completed comparison. Omit old, unchanged, unknown-date and unverified posts, even if they appeared in earlier search results. Never pad a requested maximum. Keep changed old posts separate from new posts and respect the user's requested scope. Do not search again merely to fill the report."));
            }
        }
        if let Some(limit) = comparison.report_limit {
            if cited_sources.len() as u64 > limit {
                return Err(format!("The draft monitoring report was withheld because it exceeds the user's item limit: {limit} posts total, but {} distinct posts are cited. Rewrite using at most {limit} qualified posts across the entire answer. Remove optional leads, runner-ups and extra lists, not just their links. Do not collect again; choose from the completed comparison.", cited_sources.len()));
            }
        }
        Ok(())
    }
}

/// Tracking parameters and X handle changes do not change post identity.
fn source_key(value: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url
        .host_str()?
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    let parts: Vec<_> = url.path_segments()?.collect();
    if matches!(
        host.as_str(),
        "x.com" | "twitter.com" | "mobile.twitter.com"
    ) && url.port().is_none() {
        if let Some(id) = parts.windows(2).find_map(|parts| {
            (parts[0] == "status"
                && !parts[1].is_empty()
                && parts[1].bytes().all(|b| b.is_ascii_digit()))
            .then_some(parts[1])
        }) {
            return Some(format!("x:{id}"));
        }
    }
    url.set_host(Some(&host)).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.as_str().trim_end_matches('/').to_string())
}
