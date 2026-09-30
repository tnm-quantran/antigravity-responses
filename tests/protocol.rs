use antigravity_responses::{
    config::Config,
    protocol::{Replay, tools, translate},
    schema,
    setup::{merge_codex, merge_codex_profile},
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
fn invalid_google_tool_names_are_aliased_and_restored_for_codex() {
    let request = json!({
        "model":"gemini-test",
        "input":"call it",
        "tools":[{"type":"function","name":"mcp__server__search.v2"}]
    });
    let (declarations, registry) = tools(&request).unwrap();
    let wire_name = declarations[0]["name"].as_str().unwrap();
    assert_ne!(wire_name, "mcp__server__search.v2");
    assert!(wire_name.len() <= 64);
    assert!(
        wire_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    );

    let mut stream = Translator::new("gemini-test", registry, "hidden");
    stream
        .ingest(
            &json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":wire_name,"args":{}}}]},"finishReason":"STOP"}]}),
            &mut replay(),
        )
        .unwrap();
    assert_eq!(
        stream.completed().unwrap()["response"]["output"][0]["name"],
        "mcp__server__search.v2"
    );
}

#[test]
fn web_search_maps_to_internal_function_tool() {
    let request = json!({
        "model":"gemini-test",
        "input":[
            {"role":"user","content":"search this"},
            {"type":"web_search_call","id":"previous-search"}
        ],
        "tools":[
            {"type":"web_search","external_web_access":false,"filters":{"allowed_domains":["example.com"]}},
            {"type":"function","name":"lookup","parameters":{"type":"object"}}
        ]
    });
    let body = translate(&request, &mut replay()).unwrap();
    assert_eq!(
        body["tools"],
        json!([{"functionDeclarations":[
            {"name":"gateway_web_search","description":"Search the web for current information. Provide a concise query.","parameters":{"type":"OBJECT","properties":{"query":{"type":"STRING"}},"required":["query"]}},
            {"name":"lookup","description":"","parameters":{"type":"OBJECT"}}
        ]}])
    );
    assert!(body.get("toolConfig").is_none());
    assert!(
        body["toolConfig"]
            .get("includeServerSideToolInvocations")
            .is_none()
    );
    assert_eq!(body["contents"].as_array().unwrap().len(), 1);

    for kind in ["web_search", "web_search_preview"] {
        let search_only = translate(
            &json!({"model":"gemini-test","input":"search","tools":[{"type":kind}]}),
            &mut replay(),
        )
        .unwrap();
        assert_eq!(
            search_only["tools"][0]["functionDeclarations"][0]["name"],
            "gateway_web_search"
        );
    }

    let forced = translate(
        &json!({
            "model":"gemini-test",
            "input":"lookup",
            "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
            "tool_choice":{"type":"function","name":"lookup"}
        }),
        &mut replay(),
    )
    .unwrap();
    assert_eq!(
        forced["toolConfig"]["functionCallingConfig"],
        json!({"mode":"ANY","allowedFunctionNames":["lookup"]})
    );
}

#[test]
fn tool_search_uses_its_declared_schema_and_round_trips_execution() {
    let request = json!({
        "model":"gemini-test",
        "input":"find a tool",
        "tools":[{
            "type":"tool_search",
            "execution":"client",
            "description":"Find available tools",
            "parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
        }]
    });
    let body = translate(&request, &mut replay()).unwrap();
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0],
        json!({"name":"tool_search","description":"Find available tools","parameters":{"type":"OBJECT","properties":{"query":{"type":"STRING"}},"required":["query"]}})
    );

    let mut state = replay();
    let mut stream = Translator::new("gemini-test", tools(&request).unwrap().1, "hidden");
    stream
        .ingest(
            &json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"tool_search","args":{"query":"calendar"}}}]},"finishReason":"STOP"}]}),
            &mut state,
        )
        .unwrap();
    let output = stream.completed().unwrap()["response"]["output"][0].clone();
    assert_eq!(output["type"], "tool_search_call");
    assert_eq!(output["execution"], "client");
    assert_eq!(output["arguments"]["query"], "calendar");
}

#[test]
fn generation_options_map_and_validate() {
    let request = json!({
        "model":"gemini-test", "input":"hello", "temperature":0.4, "top_p":0.8
    });
    let body = translate(&request, &mut replay()).unwrap();
    assert_eq!(body["generationConfig"]["temperature"], json!(0.4));
    assert_eq!(body["generationConfig"]["topP"], json!(0.8));
    assert!(
        translate(
            &json!({"model":"x","input":"x","temperature":3}),
            &mut replay()
        )
        .is_err()
    );
}

#[test]
fn previous_response_history_is_chained_and_bounded() {
    let mut state = replay();
    state
        .store_response("one", None, vec![json!({"role":"user","content":"first"})])
        .unwrap();
    state
        .store_response(
            "two",
            Some("one"),
            vec![json!({"role":"assistant","content":"answer"})],
        )
        .unwrap();
    let body = translate(
        &json!({"model":"test","previous_response_id":"two","input":"next"}),
        &mut state,
    )
    .unwrap();
    assert_eq!(
        body["contents"],
        json!([
            {"role":"user","parts":[{"text":"first"}]},
            {"role":"model","parts":[{"text":"answer"}]},
            {"role":"user","parts":[{"text":"next"}]}
        ])
    );
    assert!(
        translate(
            &json!({"model":"test","previous_response_id":"missing","input":"next"}),
            &mut state
        )
        .is_err()
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
            .any(|event| event["type"] == "response.reasoning_text.delta")
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
fn tool_schema_drops_google_unsupported_constraints() {
    let schema = json!({
        "type":"object",
        "additionalProperties":false,
        "properties":{"query":{"type":"string","pattern":"^[a-z]+$","minLength":2}}
    });
    assert_eq!(
        schema::translate_tool(&schema, true).unwrap(),
        json!({"type":"OBJECT","properties":{"query":{"type":"STRING"}}})
    );
}

#[test]
fn config_merge_preserves_user_settings_is_idempotent_and_rejects_conflicts() {
    let text = "# Keep this\nmodel_provider = 'existing'\nprofile = 'antigravity_responses'\n[profiles.antigravity_responses]\nmodel = 'legacy'\n[profiles.user]\nmodel = 'mine'\n";
    let merged = merge_codex(text, &config()).unwrap();
    assert!(merged.contains("# Keep this"));
    assert!(merged.contains("model_provider = 'existing'"));
    assert!(!merged.contains("profile = 'antigravity_responses'"));
    assert!(!merged.contains("[profiles.antigravity_responses]"));
    assert!(merged.contains("model = 'mine'"));
    assert_eq!(merge_codex(&merged, &config()).unwrap(), merged);
    let profile =
        merge_codex_profile("", &config(), std::path::Path::new("/tmp/models.json")).unwrap();
    assert!(profile.contains("model_provider = \"antigravity_responses\""));
    assert!(profile.contains("model = \"gemini-test\""));
    assert_eq!(
        merge_codex_profile(
            &profile,
            &config(),
            std::path::Path::new("/tmp/models.json")
        )
        .unwrap(),
        profile
    );
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
