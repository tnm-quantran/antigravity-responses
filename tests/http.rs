use antigravity_responses::{
    auth::{Credentials, save_credentials},
    config::Config,
    server::{Gateway, router},
};
use axum::{Json, Router, body::Body, extract::State, http::Response, routing::post};
use clap::Parser;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{Mutex, mpsc};

#[derive(Parser)]
struct Options {
    #[command(flatten)]
    config: Config,
}

struct Fixture {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    chunks: mpsc::Sender<String>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    credentials: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (chunks, receiver) = mpsc::channel::<String>(8);
        let receiver = Arc::new(Mutex::new(receiver));
        let backend = Router::new()
            .route(
                "/v1internal:streamGenerateContent",
                post({
                    let receiver = receiver.clone();
                    move |State(requests): State<Arc<Mutex<Vec<Value>>>>,
                          Json(body): Json<Value>| {
                        let receiver = receiver.clone();
                        async move {
                            requests.lock().await.push(body);
                            let stream = async_stream::stream! {
                                while let Some(chunk) = receiver.lock().await.recv().await {
                                    if chunk == "EOF" { break; }
                                    yield Ok::<_,Infallible>(chunk);
                                }
                            };
                            Response::builder()
                                .header("content-type", "text/event-stream")
                                .body(Body::from_stream(stream))
                                .unwrap()
                        }
                    }
                }),
            )
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_url = format!("http://{}", listener.local_addr().unwrap());
        let backend_task = tokio::spawn(async { axum::serve(listener, backend).await.unwrap() });
        let credentials =
            std::env::temp_dir().join(format!("ag-test-{}.credentials.json", uuid::Uuid::new_v4()));
        save_credentials(
            &credentials,
            &Credentials {
                access_token: "test-only".into(),
                refresh_token: None,
                expires_at: u64::MAX,
            },
        )
        .unwrap();
        let mut config = Options::parse_from([
            "test",
            "--base-url",
            &backend_url,
            "--project",
            "test-project",
            "--credentials",
            credentials.to_str().unwrap(),
        ])
        .config;
        config.access_token = None;
        let gateway = Gateway::new(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let gateway_task =
            tokio::spawn(async { axum::serve(listener, router(gateway)).await.unwrap() });
        Self {
            url,
            requests,
            chunks,
            tasks: vec![backend_task, gateway_task],
            credentials,
        }
    }

    async fn send(&self, value: Value) {
        self.chunks
            .send(format!("data: {value}\n\n"))
            .await
            .unwrap();
    }

    async fn eof(&self) {
        self.chunks.send("EOF".into()).await.unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        std::fs::remove_file(&self.credentials).expect("remove test credential file");
    }
}

#[tokio::test]
async fn http_tool_loop_preserves_signature_and_freeform_patch() {
    let fixture = Fixture::new().await;
    let client = reqwest::Client::new();
    let request = json!({"model":"gemini-3.8-flash-medium","reasoning":{"effort":"none"},"input":"fix","tools":[{"type":"custom","name":"apply_patch"}]});
    fixture.send(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"apply_patch","args":{"input":"*** Begin Patch\n*** End Patch"}},"thoughtSignature":"opaque-signature"}]},"finishReason":"STOP"}]})).await;
    fixture.eof().await;
    let response: Value = client
        .post(format!("{}/v1/responses", fixture.url))
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let call = response["output"][0].clone();
    assert_eq!(call["type"], json!("custom_tool_call"));
    fixture
        .send(
            json!({"candidates":[{"content":{"parts":[{"text":"fixed"}]},"finishReason":"STOP"}]}),
        )
        .await;
    fixture.eof().await;
    let mut next = request;
    next["input"] = json!([{"role":"user","content":"fix"},call,{"type":"custom_tool_call_output","call_id":call["call_id"],"output":"success"}]);
    let response: Value = client
        .post(format!("{}/v1/responses", fixture.url))
        .json(&next)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["output"][0]["content"][0]["text"], json!("fixed"));
    let captured = fixture.requests.lock().await;
    assert_eq!(captured.len(), 2);
    for body in captured.iter() {
        assert_eq!(body["model"], "gemini-3.8-flash-medium");
        assert_eq!(
            body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "MEDIUM"
        );
    }
    assert_eq!(
        captured[1]["request"]["contents"][1]["parts"][0]["thoughtSignature"],
        json!("opaque-signature")
    );
    assert_eq!(
        captured[1]["request"]["contents"][2]["parts"][0]["functionResponse"]["name"],
        json!("apply_patch")
    );
}

#[tokio::test]
async fn web_search_sidecar_separates_search_from_function_calls() {
    let fixture = Fixture::new().await;
    let client = reqwest::Client::new();
    fixture
        .send(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"gateway_web_search","args":{"query":"example query"}}}]},"finishReason":"STOP"}]}))
        .await;
    fixture.eof().await;
    fixture
        .send(json!({"candidates":[{"content":{"parts":[{"text":"Example answer."}]},"groundingMetadata":{"webSearchQueries":["example query"],"groundingChunks":[{"web":{"uri":"https://example.com","title":"Example"}}],"groundingSupports":[{"segment":{"startIndex":0,"endIndex":7,"text":"Example"},"groundingChunkIndices":[0]}]},"finishReason":"STOP"}]}))
        .await;
    fixture.eof().await;
    fixture
        .send(json!({"candidates":[{"content":{"parts":[{"text":"Example answer."}]},"finishReason":"STOP"}]}))
        .await;
    fixture.eof().await;
    let response: Value = client
        .post(format!("{}/v1/responses", fixture.url))
        .json(&json!({"model":"gemini-test","input":"search the web","tools":[{"type":"web_search"},{"type":"function","name":"lookup","parameters":{"type":"object"}}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["output"][0]["type"], "web_search_call");
    assert_eq!(response["output"][0]["action"]["type"], "search");
    assert_eq!(response["output"][0]["action"]["query"], "example query");
    assert_eq!(response["output"][1]["type"], "message");
    assert_eq!(response["output"].as_array().unwrap().len(), 2);
    assert!(
        response["output"][1]["content"][0]["annotations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let requests = fixture.requests.lock().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0]["request"]["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        requests[0]["request"]["toolConfig"]
            .get("includeServerSideToolInvocations")
            .is_none()
    );
    assert_eq!(
        requests[1]["request"]["tools"],
        json!([{"googleSearch":{}}])
    );
    assert_eq!(
        requests[1]["request"]["toolConfig"]["includeServerSideToolInvocations"],
        true
    );
    assert_eq!(
        requests[2]["request"]["tools"][0]["functionDeclarations"][0]["name"],
        "lookup"
    );
}

#[tokio::test]
async fn previous_response_id_continues_the_conversation() {
    let fixture = Fixture::new().await;
    let client = reqwest::Client::new();
    fixture
        .send(json!({"candidates":[{"content":{"parts":[{"text":"first answer"}]},"finishReason":"STOP"}]}))
        .await;
    fixture.eof().await;
    let first: Value = client
        .post(format!("{}/v1/responses", fixture.url))
        .json(&json!({"model":"test","input":"first prompt"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    fixture
        .send(json!({"candidates":[{"content":{"parts":[{"text":"second answer"}]},"finishReason":"STOP"}]}))
        .await;
    fixture.eof().await;
    let _: Value = client
        .post(format!("{}/v1/responses", fixture.url))
        .json(&json!({"model":"test","previous_response_id":first["id"],"input":"second prompt"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    let captured = fixture.requests.lock().await;
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[1]["request"]["contents"],
        json!([
            {"role":"user","parts":[{"text":"first prompt"}]},
            {"role":"model","parts":[{"text":"first answer"}]},
            {"role":"user","parts":[{"text":"second prompt"}]}
        ])
    );
}

#[tokio::test]
async fn sse_delta_arrives_before_backend_completion() {
    let fixture = Fixture::new().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", fixture.url))
        .json(&json!({"model":"test","input":"hello","stream":true}))
        .send()
        .await
        .unwrap();
    let mut bytes = response.bytes_stream();
    let initial = tokio::time::timeout(Duration::from_secs(2), bytes.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&initial).contains("response.created"));
    fixture
        .send(json!({"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}))
        .await;
    let mut text = String::new();
    while !text.contains("response.output_text.delta") {
        let chunk = tokio::time::timeout(Duration::from_secs(2), bytes.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(!text.contains("response.completed"));
    fixture
        .send(json!({"candidates":[{"finishReason":"STOP"}]}))
        .await;
    fixture.eof().await;
    while let Some(chunk) = bytes.next().await {
        text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    assert!(text.contains("response.completed"));
}

#[tokio::test]
async fn mcp_call_streams_before_completion_with_web_search_enabled() {
    let fixture = Fixture::new().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", fixture.url))
        .json(&json!({"model":"test","input":"inspect","stream":true,"tools":[{"type":"web_search"},{"type":"function","name":"mcp__apps__inspect"}]}))
        .send().await.unwrap().error_for_status().unwrap();
    let mut bytes = response.bytes_stream();
    let initial = tokio::time::timeout(Duration::from_secs(2), bytes.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&initial).contains("response.created"));
    fixture.send(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"mcp__apps__inspect","args":{}},"thoughtSignature":"signed"}]}}]})).await;
    let mut text = String::new();
    while !text.contains("response.output_item.done") {
        let chunk = tokio::time::timeout(Duration::from_secs(2), bytes.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(text.contains("mcp__apps__inspect"));
    assert!(!text.contains("response.completed"));
    fixture
        .send(json!({"candidates":[{"finishReason":"STOP"}]}))
        .await;
    fixture.eof().await;
    while let Some(chunk) = bytes.next().await {
        text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    assert!(text.contains("response.completed"));
    assert!(!text.contains("response.failed"));
    assert_eq!(text.matches("event: response.output_item.done").count(), 1);
    assert_eq!(fixture.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn mixed_search_and_mcp_calls_are_returned_once_in_either_order() {
    for is_search_first in [false, true] {
        let fixture = Fixture::new().await;
        let mut search =
            json!({"functionCall":{"name":"gateway_web_search","args":{"query":"example"}}});
        let mut mcp = json!({"functionCall":{"name":"mcp__apps__inspect","args":{}}});
        if is_search_first {
            search["thoughtSignature"] = json!("signed");
        } else {
            mcp["thoughtSignature"] = json!("signed");
        }
        let parts = if is_search_first {
            vec![
                json!({"text":"before","thought":true}),
                search,
                mcp,
                json!({"text":"after"}),
            ]
        } else {
            vec![
                json!({"text":"before","thought":true}),
                mcp,
                search,
                json!({"text":"after"}),
            ]
        };
        fixture
            .send(json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]}))
            .await;
        fixture.eof().await;
        fixture.send(json!({"candidates":[{"content":{"parts":[{"text":"found"}]},"finishReason":"STOP"}]})).await;
        fixture.eof().await;
        let response: Value = reqwest::Client::new().post(format!("{}/v1/responses", fixture.url))
            .json(&json!({"model":"test","input":"inspect","tools":[{"type":"web_search"},{"type":"function","name":"mcp__apps__inspect"}]}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let output = response["output"].as_array().unwrap();
        assert_eq!(output.len(), 4);
        let call = output
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert_eq!(call["name"], "mcp__apps__inspect");
        assert_eq!(
            call["extra_content"]["google"]["thought_signature"],
            if is_search_first {
                Value::Null
            } else {
                json!("signed")
            }
        );
        assert_eq!(
            output
                .iter()
                .filter(|item| item["type"] == "web_search_call")
                .count(),
            1
        );
        assert_eq!(fixture.requests.lock().await.len(), 2);
        for uses_previous_response in [true, false] {
            fixture.send(json!({"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}]})).await;
            fixture.eof().await;
            let mut next = json!({"model":"test","tools":[{"type":"web_search"},{"type":"function","name":"mcp__apps__inspect"}],"input":[{"type":"function_call_output","call_id":call["call_id"],"output":"ok"}]});
            if uses_previous_response {
                next["previous_response_id"] = response["id"].clone();
            } else {
                let mut history = vec![json!({"role":"user","content":"inspect"})];
                history.extend(output.clone());
                history.push(next["input"][0].clone());
                next["input"] = json!(history);
            }
            reqwest::Client::new()
                .post(format!("{}/v1/responses", fixture.url))
                .json(&next)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            let requests = fixture.requests.lock().await;
            let contents = requests.last().unwrap()["request"]["contents"]
                .as_array()
                .unwrap();
            assert_eq!(contents.len(), 3);
            let parts = contents[1]["parts"].as_array().unwrap();
            assert_eq!(parts.len(), 4);
            assert_eq!(parts[0]["text"], "before");
            assert_eq!(parts[3]["text"], "after");
            assert_eq!(parts[1]["thoughtSignature"], "signed");
            assert_eq!(
                parts[1]["functionCall"]["name"],
                if is_search_first {
                    "gateway_web_search"
                } else {
                    "mcp__apps__inspect"
                }
            );
            assert_eq!(contents[2]["parts"].as_array().unwrap().len(), 2);
            assert!(
                contents[2]["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|part| part.get("functionResponse").is_some())
            );
        }
    }
}

#[tokio::test]
async fn invalid_input_is_rejected_before_backend_and_truncated_stream_fails() {
    let fixture = Fixture::new().await;
    let client = reqwest::Client::new();
    let response = client.post(format!("{}/v1/responses",fixture.url)).json(&json!({"model":"test","input":[{"type":"function_call_output","call_id":"missing","output":"x"}]})).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert!(fixture.requests.lock().await.is_empty());
    fixture
        .send(json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}))
        .await;
    fixture.eof().await;
    let response = client
        .post(format!("{}/v1/responses", fixture.url))
        .json(&json!({"model":"test","input":"x","stream":true}))
        .send()
        .await
        .unwrap();
    let text = response.text().await.unwrap();
    assert!(text.contains("response.failed"));
    assert!(!text.contains("response.completed"));
}
