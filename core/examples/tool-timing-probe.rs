//! Offline diagnostic: exercise the real collector and OTLP serializer.
//! No browser, model, credentials or production directory is used.
use serde_json::json;
use socai_core::agent::{TokenUsage, ToolContext};
use socai_core::telemetry::trace::RunTraceBuilder;

fn main() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let mut trace = RunTraceBuilder::new(
        temporary.path(),
        "11111111-1111-4111-8111-111111111111",
        "offline diagnostic",
        "test",
        "test",
        None,
        0,
    );
    let ctx = ToolContext::new("11111111-1111-4111-8111-111111111111", temporary.path())
        .with_tool_dir(temporary.path().join("first"));
    ctx.start_tool_timing();
    let (post, outer) = ctx.timing_scope("read_post", Some("1234567890123456789"), Some(1));
    let (_, inner) = post.timing_scope("read_detail", None, None);
    inner.finish(true);
    let (_, cancelled) = post.timing_scope("collect_comments", None, None);
    drop(cancelled);
    outer.finish(false);
    let isolated = ctx.clone().with_tool_dir(temporary.path().join("second"));
    let (_, separate) = isolated.timing_scope("navigate", Some("https://private.test/token"), None);
    separate.finish(true);
    ctx.finish_tool_timing();
    let batch = ctx.take_tool_timings();
    let phases = &batch.phases;
    assert_eq!(phases.len(), 3);
    let parent = phases.iter().find(|p| p.phase == "read_post").unwrap();
    for phase in phases.iter().filter(|p| p.phase != "read_post") {
        assert_eq!(phase.parent.as_deref(), Some(parent.id.as_str()));
        assert!(phase.start_ns >= parent.start_ns && phase.end_ns <= parent.end_ns);
    }
    assert!(
        !phases
            .iter()
            .find(|p| p.phase == "collect_comments")
            .unwrap()
            .succeeded
    );
    assert!(ctx.take_tool_timings().phases.is_empty());
    let separate = isolated.take_tool_timings().phases;
    assert_eq!(separate.len(), 1);
    assert!(separate[0].parent.is_none() && separate[0].post_id.is_none());
    trace.record_tool(1, 1, "search", 1, &json!({}), &json!([]), None);
    trace.record_tool_phases(1, 1, "search", batch);
    trace.finish("completed", 1, &TokenUsage::default(), None, None);
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temporary.path().join("trace.json"))?)?;
    let spans = value
        .pointer("/resourceSpans/0/scopeSpans/0/spans")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(spans.len(), 5);
    let ids: std::collections::HashSet<_> =
        spans.iter().filter_map(|s| s["spanId"].as_str()).collect();
    assert!(spans
        .iter()
        .filter_map(|s| s["parentSpanId"].as_str())
        .all(|id| ids.contains(id)));
    for child in spans.iter().filter(|s| s["parentSpanId"].is_string()) {
        let parent = spans
            .iter()
            .find(|s| s["spanId"] == child["parentSpanId"])
            .unwrap();
        let ns =
            |s: &serde_json::Value, key: &str| s[key].as_str().unwrap().parse::<u64>().unwrap();
        assert!(ns(parent, "startTimeUnixNano") <= ns(child, "startTimeUnixNano"));
        assert!(ns(parent, "endTimeUnixNano") >= ns(child, "endTimeUnixNano"));
    }
    assert!(!value.to_string().contains("private.test"));
    // The collector and final trace have independent bounds. Neither may
    // silently turn a partial timeline into an apparently complete trace.
    let bounded = ctx.clone().with_tool_dir(temporary.path().join("bounded"));
    for _ in 0..305 {
        let (_, phase) = bounded.timing_scope("browser_extract", None, None);
        phase.finish(true);
    }
    let bounded_batch = bounded.take_tool_timings();
    assert_eq!(bounded_batch.phases.len(), 300);
    assert_eq!(bounded_batch.dropped, 5);
    let cap_dir = temporary.path().join("cap");
    std::fs::create_dir(&cap_dir)?;
    let mut cap_trace = RunTraceBuilder::new(
        &cap_dir,
        "22222222-2222-4222-8222-222222222222",
        "offline cap diagnostic",
        "test",
        "test",
        None,
        0,
    );
    for sequence in 1..=398 {
        cap_trace.record_tool(1, sequence, "noop", 1, &json!({}), &json!([]), None);
    }
    cap_trace.record_tool(1, 399, "search", 1, &json!({}), &json!([]), None);
    cap_trace.record_tool_phases(1, 399, "search", bounded_batch);
    cap_trace.finish("completed", 1, &TokenUsage::default(), None, None);
    let cap: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cap_dir.join("trace.json"))?)?;
    let cap_spans = cap
        .pointer("/resourceSpans/0/scopeSpans/0/spans")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(cap_spans.len(), 401); // 400 children plus the root.
    let tool = cap_spans
        .iter()
        .find(|s| s["name"] == "execute_tool search")
        .unwrap();
    assert!(tool["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["key"] == "socai.phases_dropped" && a["value"]["intValue"] == "304"));
    let cap_ids: std::collections::HashSet<_> = cap_spans
        .iter()
        .filter_map(|s| s["spanId"].as_str())
        .collect();
    assert!(cap_spans
        .iter()
        .filter_map(|s| s["parentSpanId"].as_str())
        .all(|id| cap_ids.contains(id)));
    println!(
        "{}",
        json!({"collector_phases":3,"serialized_spans":spans.len(),"nested_parent_and_bounds_check":true,"cancelled_scope_check":true,"call_context_isolation_check":true,"private_input_omitted":true,"collector_limit":300,"trace_child_limit":400,"omitted_phases_reported":304,"model_calls":0,"browser_calls":0})
    );
    Ok(())
}
