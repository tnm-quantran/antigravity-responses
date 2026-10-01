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
fn selected_gemini_slug_controls_thinking_level_even_with_conflicting_effort() {
    for (model, level) in [
        ("gemini-3.6-flash-low", "LOW"),
        ("gemini-3.7-flash-medium", "MEDIUM"),
        ("gemini-3.8-flash-high", "HIGH"),
    ] {
        for effort in [None, Some("none"), Some("low"), Some("high")] {
            let mut request = json!({"model":model,"input":"test"});
            if let Some(effort) = effort {
                request["reasoning"] = json!({"effort":effort});
            }
            let body = translate(&request, &mut replay()).unwrap();
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                level
            );
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["includeThoughts"],
                true
            );
        }
    }
    let body = translate(
        &json!({"model":"gemini-test","input":"test","reasoning":{"effort":"medium"}}),
        &mut replay(),
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "MEDIUM"
    );
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
fn web_search_cache_only_is_rejected_without_dropping_the_tool() {
    let request = json!({"model":"gemini-test","input":"search","tools":[{
        "type":"web_search","external_web_access":false
    }]});
    let error = translate(&request, &mut replay()).unwrap_err();
    assert!(error.to_string().contains("cache-only"));

    let mut preview = request;
    preview["tools"][0]["type"] = json!("web_search_preview");
    let body = translate(&preview, &mut replay()).unwrap();
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "gateway_web_search"
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
            {"type":"web_search","filters":{"allowed_domains":["example.com"]}},
            {"type":"function","name":"lookup","parameters":{"type":"object"}}
        ]
    });
    let body = translate(&request, &mut replay()).unwrap();
    assert_eq!(
        body["tools"],
        json!([{"functionDeclarations":[
            {"name":"gateway_web_search","description":"Search the web or inspect a user-provided URL. Provide the URL or a concise search query.","parameters":{"type":"OBJECT","properties":{"query":{"type":"STRING"}},"required":["query"]}},
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
fn previous_response_id_requires_full_history_in_input() {
    let error = translate(
        &json!({"model":"test","previous_response_id":"old","input":"next"}),
        &mut replay(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("previous_response_id unsupported")
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
    assert_eq!(
        output[0]["summary"],
        json!([{"type":"summary_text","text":"inspect first"}])
    );
    assert!(events.iter().any(|event| {
        event["type"] == "response.reasoning_summary_text.done"
            && event["text"] == output[0]["summary"][0]["text"]
    }));
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
fn empty_backend_text_is_skipped_but_signed_empty_parts_remain_replayable() {
    let request = json!({"input":"lookup","tools":[{"type":"function","name":"lookup"}]});
    let mut state = replay();
    let mut stream = Translator::new("test", tools(&request).unwrap().1, "raw-thought");
    let empty =
        json!({"candidates":[{"content":{"parts":[{"text":""},{"text":"","thought":true}]}}]});
    assert!(stream.ingest(&empty, &mut state).unwrap().is_empty());
    let signed = json!({"text":"","thoughtSignature":"empty-signature"});
    let events = stream.ingest(&json!({"candidates":[{"content":{"parts":[signed,{"functionCall":{"name":"lookup","args":{}}},{"text":""}]},"finishReason":"STOP"}]}), &mut state).unwrap();
    assert!(!events.iter().any(|event| {
        event["type"] == "response.output_text.delta"
            || event["type"] == "response.content_part.added"
            || event["type"] == "response.reasoning_summary_part.added"
    }));
    let output = stream.completed().unwrap()["response"]["output"].clone();
    assert_eq!(output.as_array().unwrap().len(), 2);
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[1]["type"], "function_call");
    let mut input = vec![json!({"role":"user","content":"lookup"})];
    input.extend(output.as_array().unwrap().clone());
    input.push(
        json!({"type":"function_call_output","call_id":output[1]["call_id"],"output":"found"}),
    );
    let body = translate(&json!({"input":input}), &mut state).unwrap();
    assert_eq!(body["contents"][1]["parts"][0], signed);
    assert_eq!(body["contents"][1]["parts"].as_array().unwrap().len(), 2);
}

#[test]
fn progress_and_final_answer_have_distinct_phases_with_signed_stream_tails() {
    let request = json!({"input":"lookup","tools":[{"type":"function","name":"lookup"}]});
    let mut state = replay();
    let mut progress = Translator::new("test", tools(&request).unwrap().1, "raw-thought");
    progress.ingest(&json!({"candidates":[{"content":{"parts":[{"text":"checking"},{"functionCall":{"name":"lookup","args":{}}}]},"finishReason":"STOP"}]}), &mut state).unwrap();
    assert_eq!(progress.response["output"][0]["phase"], "commentary");

    let mut answer = Translator::new("test", Default::default(), "raw-thought");
    answer
        .ingest(
            &json!({"candidates":[{"content":{"parts":[{"text":"done"}]}}]}),
            &mut state,
        )
        .unwrap();
    let signature = json!({"text":"","thoughtSignature":"tail-signature"});
    let events = answer
        .ingest(
            &json!({"candidates":[{"content":{"parts":[signature]},"finishReason":"STOP"}]}),
            &mut state,
        )
        .unwrap();
    let output = &answer.response["output"];
    assert_eq!(output.as_array().unwrap().len(), 1);
    assert_eq!(output[0]["phase"], "final_answer");
    assert!(events.iter().any(|event| {
        event["type"] == "response.output_item.done" && event["item"]["phase"] == "final_answer"
    }));
    let body = translate(&json!({"input":output}), &mut state).unwrap();
    assert_eq!(
        body["contents"][0]["parts"],
        json!([{"text":"done"},signature])
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
fn replay_memory_budget_evicts_old_state() {
    let mut state = Replay::new(30, Duration::from_secs(60));
    for key in ["one", "two", "three"] {
        state.insert(key.into(), json!("0123456789")).unwrap();
    }
    assert!(state.get("one").is_err());
    assert_eq!(state.get("three").unwrap(), json!("0123456789"));
    assert!(state.get("missing").is_err());
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
        json!({"type":"OBJECT","properties":{"query":{"type":"STRING","pattern":"^[a-z]+$","minLength":2}}})
    );
}

#[test]
fn tool_signature_survives_reconstruction_without_replay() {
    let request = json!({"model":"gemini-test","input":"inspect","tools":[{"type":"namespace","name":"mcp__apps__","tools":[{"type":"function","name":"inspect"}]}]});
    let mut stream = Translator::new("gemini-test", tools(&request).unwrap().1, "hidden");
    stream.ingest(&json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"mcp__apps__inspect","args":{}},"thoughtSignature":"signed"}]},"finishReason":"STOP"}]}), &mut replay()).unwrap();
    let call = stream.completed().unwrap()["response"]["output"][0].clone();
    assert_eq!(
        call["extra_content"]["google"]["thought_signature"],
        "signed"
    );
    let mut next = request;
    next["input"] = json!([{"role":"user","content":"inspect"},call,{"type":"function_call_output","call_id":call["call_id"],"output":"ok"}]);
    let body = translate(&next, &mut replay()).unwrap();
    assert_eq!(
        body["contents"][1]["parts"][0]["thoughtSignature"],
        "signed"
    );
    assert!(
        body["contents"][1]["parts"][0]["functionCall"]
            .get("thoughtSignature")
            .is_none()
    );
}

#[test]
fn mcp_image_output_is_media_and_preserves_text() {
    let request = json!({"model":"gemini-test","tools":[{"type":"function","name":"mcp__screen__capture"}],"input":[
        {"role":"user","content":"inspect"},
        {"type":"function_call","name":"mcp__screen__capture","call_id":"capture","arguments":"{}"},
        {"type":"function_call_output","call_id":"capture","output":[{"type":"input_text","text":"screenshot"},{"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}]}
    ]});
    let body = translate(&request, &mut replay()).unwrap();
    let response = &body["contents"][2]["parts"][0]["functionResponse"];
    assert_eq!(
        response["response"]["output"][0],
        request["input"][2]["output"][0]
    );
    assert_eq!(
        response["parts"][0]["inlineData"],
        json!({"mimeType":"image/png","data":"aGVsbG8=","displayName":"tool_image_1"})
    );
    assert_eq!(
        response["response"]["output"][1],
        json!({"$ref":"tool_image_1"})
    );
    let mut invalid = request;
    invalid["input"][2]["output"][1]["image_url"] = json!("https://example.com/image.png");
    assert!(translate(&invalid, &mut replay()).is_err());
}

#[test]
fn schema_preserves_supported_bounds_and_string_const() {
    let schema = json!({"type":"object","properties":{
        "mode":{"const":"read"},
        "limit":{"type":"integer","minimum":1,"maximum":10},
        "tags":{"type":"array","minItems":1,"maxItems":3,"items":{"type":"string","minLength":2,"maxLength":8,"pattern":"^[a-z]+$"}}
    },"additionalProperties":false});
    let (translated, dropped) = schema::translate_tool_with_report(&schema, true).unwrap();
    assert_eq!(
        translated["properties"]["mode"],
        json!({"type":"STRING","enum":["read"]})
    );
    assert_eq!(
        translated["properties"]["limit"],
        json!({"type":"INTEGER","minimum":1,"maximum":10})
    );
    assert_eq!(translated["properties"]["tags"]["minItems"], 1);
    assert_eq!(
        translated["properties"]["tags"]["items"]["pattern"],
        "^[a-z]+$"
    );
    assert_eq!(dropped, vec!["additionalProperties"]);
    assert!(schema::translate_tool(&json!({"const":"read","enum":["write"]}), true).is_err());
    assert_eq!(
        schema::translate_tool(&json!({"type":["string","null"],"const":"read"}), true).unwrap(),
        json!({"type":"STRING","enum":["read"]})
    );
    assert!(schema::translate_tool(&json!({"type":"integer","const":"read"}), true).is_err());
}

#[test]
fn raw_mcp_image_output_preserves_structured_result_and_rejects_bad_base64() {
    let mut request = json!({"model":"gemini-test","tools":[{"type":"function","name":"mcp__screen__capture"}],"input":[
        {"type":"function_call","name":"mcp__screen__capture","call_id":"capture","arguments":"{}"},
        {"type":"function_call_output","call_id":"capture","output":{"content":[{"type":"text","text":"screenshot"},{"type":"image","mimeType":"image/png","data":"aGVsbG8="}],"structuredContent":{"width":100},"isError":false}}
    ]});
    let body = translate(&request, &mut replay()).unwrap();
    let response = &body["contents"][1]["parts"][0]["functionResponse"];
    assert_eq!(
        response["response"]["output"]["structuredContent"],
        json!({"width":100})
    );
    assert_eq!(response["response"]["output"]["isError"], false);
    assert_eq!(
        response["response"]["output"]["content"][1],
        json!({"$ref":"tool_image_1"})
    );
    assert_eq!(response["parts"][0]["inlineData"]["data"], "aGVsbG8=");
    request["input"][1]["output"]["content"][1]["data"] = json!("not-base64!");
    assert!(translate(&request, &mut replay()).is_err());
}

#[test]
fn grounding_after_streamed_text_keeps_output_indices_consistent() {
    let mut state = replay();
    let mut stream = Translator::new("test", Default::default(), "hidden");
    let mut events = stream
        .ingest(
            &json!({"candidates":[{"content":{"parts":[{"text":"before"}]}}]}),
            &mut state,
        )
        .unwrap();
    events.extend(stream.ingest(&json!({"candidates":[{"groundingMetadata":{"webSearchQueries":["query"]},"content":{"parts":[{"text":"after"}]},"finishReason":"STOP"}]}), &mut state).unwrap());
    let output = stream.completed().unwrap()["response"]["output"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(output.len(), 3);
    for (index, item) in output.iter().enumerate() {
        for kind in ["response.output_item.added", "response.output_item.done"] {
            let event = events
                .iter()
                .find(|event| event["type"] == kind && event["item"]["id"] == item["id"])
                .unwrap();
            assert_eq!(event["output_index"], index);
        }
    }
}

#[test]
fn tool_search_discovery_registers_and_calls_new_mcp_tool() {
    let search = json!({"type":"tool_search","execution":"client","parameters":{"type":"object","properties":{"query":{"type":"string"}}}});
    let discovered = json!({"type":"namespace","name":"mcp__apps__","tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"id":{"type":"string"}}}}]});
    let mut state = replay();
    let mut stream = Translator::new(
        "gemini-test",
        tools(&json!({"tools":[search]})).unwrap().1,
        "hidden",
    );
    stream.ingest(&json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"tool_search","args":{"query":"lookup"}},"thoughtSignature":"search-signed"}]},"finishReason":"STOP"}]}), &mut state).unwrap();
    let search_call = stream.completed().unwrap()["response"]["output"][0].clone();
    let request = json!({"model":"gemini-test","tools":[search,discovered],"input":[{"role":"user","content":"lookup"},search_call,{"type":"tool_search_output","call_id":search_call["call_id"],"tools":[discovered]}]});
    let body = translate(&request, &mut state).unwrap();
    assert_eq!(
        body["tools"][0]["functionDeclarations"][1]["name"],
        "mcp__apps__lookup"
    );
    assert_eq!(
        body["contents"][1]["parts"][0]["thoughtSignature"],
        "search-signed"
    );
    let mut stream = Translator::new("gemini-test", tools(&request).unwrap().1, "hidden");
    stream.ingest(&json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"mcp__apps__lookup","args":{"id":"42"}},"thoughtSignature":"lookup-signed"}]},"finishReason":"STOP"}]}), &mut state).unwrap();
    let call = stream.completed().unwrap()["response"]["output"][0].clone();
    assert_eq!(call["namespace"], "mcp__apps__");
    let mut next = request;
    next["input"].as_array_mut().unwrap().extend([
        call.clone(),
        json!({"type":"function_call_output","call_id":call["call_id"],"output":"found"}),
    ]);
    let body = translate(&next, &mut state).unwrap();
    assert_eq!(
        body["contents"][4]["parts"][0]["functionResponse"]["response"]["output"],
        "found"
    );
}

#[test]
fn excluded_recursive_tool_schema_does_not_block_allowed_tool() {
    let request = json!({"model":"test","input":"lookup","tool_choice":{"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"lookup"}]},"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}},{"type":"function","name":"unused","parameters":{"type":"object","$defs":{"Node":{"type":"object","properties":{"next":{"$ref":"#/$defs/Node"}}}},"properties":{"node":{"$ref":"#/$defs/Node"}}}}]});
    let body = translate(&request, &mut replay()).unwrap();
    assert_eq!(
        body["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "lookup"
    );
}

#[test]
fn image_base64_validation_handles_chunk_boundaries_and_padding() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let encoded = STANDARD.encode(vec![0_u8; 9000]);
    let mut request = json!({"model":"test","input":[{"role":"user","content":[{"type":"input_image","image_url":format!("data:image/png;base64,{encoded}")}]}]});
    let body = translate(&request, &mut replay()).unwrap();
    assert_eq!(
        body["contents"][0]["parts"][0]["inlineData"]["data"],
        encoded
    );
    let invalid = format!("{}YQ==", "YQ==".repeat(1024));
    request["input"][0]["content"][0]["image_url"] =
        json!(format!("data:image/png;base64,{invalid}"));
    assert!(translate(&request, &mut replay()).is_err());
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
    assert!(profile.contains("model_reasoning_effort = \"medium\""));
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
