use antigravity_responses::{
    config::Config,
    protocol::{Replay, tools, translate},
    schema,
    setup::merge_codex,
    stream::Translator,
};
use clap::Parser;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Parser)]
struct Options {
    #[command(flatten)]
    config: Config,
}

fn config() -> Config {
    Options::parse_from(["test", "--model", "gemini-test"]).config
}

fn replay() -> Replay {
    Replay::new(1024 * 1024, Duration::from_secs(60))
}

#[test]
fn tools_and_system_policy_keep_their_boundaries() {
    let request = json!({"model":"gemini-test","instructions":"base","input":[{"role":"developer","content":"policy"},{"role":"user","content":"task"}],"tools":[{"type":"namespace","name":"mcp__apps__","tools":[{"type":"function","name":"search","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}]},{"type":"custom","name":"apply_patch","format":{"definition":"patch grammar"}}]});
    let body = translate(&request, &mut replay()).unwrap();
    assert_eq!(
        body["systemInstruction"],
        json!({"parts":[{"text":"base"},{"text":"policy"}]})
    );
    assert_eq!(
        body["contents"],
        json!([{"role":"user","parts":[{"text":"task"}]}])
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0],
        json!({"name":"mcp__apps__search","description":"","parameters":{"type":"OBJECT","properties":{"query":{"type":"STRING"}},"required":["query"]}})
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][1]["parameters"]["properties"]["input"],
        json!({"type":"STRING"})
    );
}

#[test]
fn signed_reasoning_and_parallel_custom_tools_round_trip() {
    let request = json!({"model":"gemini-test","input":"fix","tools":[{"type":"custom","name":"apply_patch"},{"type":"namespace","name":"mcp__apps__","tools":[{"type":"function","name":"search"}]}]});
    let mut state = replay();
    let mut stream = Translator::new("gemini-test", tools(&request).unwrap().1, "raw-thought");
    let backend = json!({"response":{"candidates":[{"content":{"parts":[{"text":"inspect first","thought":true,"thoughtSignature":"reasoning-signature"},{"functionCall":{"name":"apply_patch","args":{"input":"*** Begin Patch\n*** End Patch"}},"thoughtSignature":"patch-signature"},{"functionCall":{"name":"mcp__apps__search","args":{"query":"x"}},"thoughtSignature":"search-signature"}]},"finishReason":"STOP"}]}});
    let events = stream.ingest(&backend, &mut state).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.reasoning_summary_text.delta")
    );
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "response.output_text.delta")
    );
    let output = stream.completed().unwrap()["response"]["output"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(output[1]["type"], json!("custom_tool_call"));
    assert_eq!(output[2]["namespace"], json!("mcp__apps__"));
    let mut input = vec![json!({"role":"user","content":"fix"})];
    input.extend(output.clone());
    input.push(
        json!({"type":"custom_tool_call_output","call_id":output[1]["call_id"],"output":"patched"}),
    );
    input.push(
        json!({"type":"function_call_output","call_id":output[2]["call_id"],"output":"found"}),
    );
    let body = translate(&json!({"model":"gemini-test","input":input}), &mut state).unwrap();
    assert_eq!(
        body["contents"][1]["parts"][0],
        backend["response"]["candidates"][0]["content"]["parts"][0]
    );
    assert_eq!(
        body["contents"][1]["parts"][1]["thoughtSignature"],
        json!("patch-signature")
    );
    assert_eq!(
        body["contents"][1]["parts"][2]["thoughtSignature"],
        json!("search-signature")
    );
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"],
        json!({"name":"apply_patch","id":output[1]["call_id"],"response":{"output":"patched"}})
    );
    assert_eq!(
        body["contents"][2]["parts"][1]["functionResponse"]["name"],
        json!("mcp__apps__search")
    );
}

#[test]
fn interleaved_reasoning_does_not_enter_output_text_and_preserves_message_signature() {
    let mut state = replay();
    let mut stream = Translator::new("test", Default::default(), "raw-thought");
    let events = stream.ingest(&json!({"candidates":[{"content":{"parts":[{"text":"answer","thoughtSignature":"answer-signature"},{"text":"thought","thought":true},{"text":"again"}]},"finishReason":"STOP"}]}),&mut state).unwrap();
    let deltas: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "response.output_text.delta")
        .map(|event| &event["delta"])
        .collect();
    assert_eq!(deltas, vec![&json!("answer"), &json!("again")]);
    let items = stream.response["output"].as_array().unwrap().clone();
    let body = translate(&json!({"input":items}), &mut state).unwrap();
    assert_eq!(
        body["contents"][0]["parts"],
        json!([{"text":"answer","thoughtSignature":"answer-signature"},{"text":"thought","thought":true},{"text":"again"}])
    );
}

#[test]
fn hidden_reasoning_remains_replayable() {
    let mut state = replay();
    let mut stream = Translator::new("test", Default::default(), "hidden");
    let events = stream.ingest(&json!({"candidates":[{"content":{"parts":[{"text":"private","thought":true,"thoughtSignature":"signed"}]},"finishReason":"STOP"}]}),&mut state).unwrap();
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "response.reasoning_summary_text.delta")
    );
    assert_eq!(stream.response["output"][0]["summary"], json!([]));
    let body = translate(&json!({"input":stream.response["output"]}), &mut state).unwrap();
    assert_eq!(
        body["contents"][0]["parts"],
        json!([{"text":"private","thought":true,"thoughtSignature":"signed"}])
    );
}

#[test]
fn truncated_stream_cannot_be_completed() {
    let mut stream = Translator::new("test", Default::default(), "raw-thought");
    stream
        .ingest(
            &json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}),
            &mut replay(),
        )
        .unwrap();
    assert!(stream.completed().is_err());
    assert_eq!(stream.failed()["response"]["status"], json!("failed"));
}

#[test]
fn safety_errors_and_unknown_tools_fail_explicitly() {
    for backend in [
        json!({"error":{"message":"denied"}}),
        json!({"candidates":[{"finishReason":"SAFETY"}]}),
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"unknown"}}]}}]}),
    ] {
        assert!(
            Translator::new("test", Default::default(), "hidden")
                .ingest(&backend, &mut replay())
                .is_err()
        );
    }
}

#[test]
fn replay_is_bounded_and_missing_state_is_not_fabricated() {
    let mut state = Replay::new(30, Duration::from_secs(60));
    state.insert("one".into(), json!("0123456789")).unwrap();
    state.insert("two".into(), json!("0123456789")).unwrap();
    state.insert("three".into(), json!("0123456789")).unwrap();
    assert!(state.get("one").is_err());
    assert_eq!(state.get("three").unwrap(), json!("0123456789"));
    assert!(
        translate(
            &json!({"input":[{"type":"function_call","call_id":"unknown"}]}),
            &mut state
        )
        .is_err()
    );
}

#[test]
fn schema_refs_are_inlined_and_cycles_rejected() {
    let schema = json!({"type":"object","$defs":{"X":{"type":["string","null"]}},"properties":{"x":{"$ref":"#/$defs/X"}},"additionalProperties":false});
    assert_eq!(
        schema::translate(&schema, true).unwrap(),
        json!({"type":"OBJECT","properties":{"x":{"type":"STRING","nullable":true}}})
    );
    assert!(
        schema::translate(
            &json!({"$ref":"#/$defs/X","$defs":{"X":{"$ref":"#/$defs/X"}}}),
            true
        )
        .is_err()
    );
}

#[test]
fn config_merge_preserves_user_settings_is_idempotent_and_rejects_conflicts() {
    let text = "# Keep this\nmodel_provider = 'existing'\n[profiles.user]\nmodel = 'mine'\n";
    let merged = merge_codex(text, &config()).unwrap();
    assert!(merged.contains("# Keep this"));
    assert!(merged.contains("model_provider = 'existing'"));
    assert!(merged.contains("model = 'mine'"));
    assert_eq!(merge_codex(&merged, &config()).unwrap(), merged);
    assert!(
        merge_codex(
            "[model_providers.antigravity_responses]\nbase_url='https://elsewhere'",
            &config()
        )
        .is_err()
    );
}

#[test]
fn pkce_uses_s256_and_keeps_callback_state() {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let url = antigravity_responses::auth::authorization_url(
        "client",
        "http://127.0.0.1/callback",
        "state",
        verifier,
    )
    .unwrap();
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        query["code_challenge"],
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    assert_eq!(query["state"], "state");
}
