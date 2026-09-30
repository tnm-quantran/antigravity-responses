use crate::{
    auth,
    config::Config,
    protocol::{Replay, tools, translate},
    stream::Translator,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct Gateway {
    pub config: Config,
    client: reqwest::Client,
    replay: Mutex<Replay>,
    credentials: Mutex<auth::TokenCache>,
    project: Mutex<Option<String>>,
}

impl Gateway {
    pub fn new(config: Config) -> Result<Arc<Self>> {
        config.validate()?;
        Ok(Arc::new(Self {
            client: config.client()?,
            replay: Mutex::new(Replay::new(
                config.state_bytes,
                Duration::from_secs(config.state_ttl_seconds),
            )),
            project: Mutex::new(config.project.clone()),
            credentials: Mutex::new(auth::TokenCache::default()),
            config,
        }))
    }

    async fn project(&self, token: &str) -> Result<String> {
        let mut cached = self.project.lock().await;
        if let Some(project) = &*cached {
            return Ok(project.clone());
        }
        let url = format!(
            "{}/v1internal:loadCodeAssist",
            self.config.base_url.trim_end_matches('/')
        );
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .header("User-Agent", &self.config.user_agent)
            .header(
                "client-metadata",
                "ideType=ANTIGRAVITY,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI",
            )
            .json(&json!({"metadata":{"ideType":"ANTIGRAVITY"}}))
            .send()
            .await?
            .error_for_status()?;
        let value: Value = response.json().await?;
        let project = value["cloudaicompanionProject"].as_str().filter(|project| !project.is_empty())
            .context("account has no Code Assist project; onboard with Antigravity or set ANTIGRAVITY_PROJECT")?.to_owned();
        *cached = Some(project.clone());
        Ok(project)
    }
}

pub fn router(gateway: Arc<Gateway>) -> Router {
    let limit = gateway.config.request_bytes;
    Router::new()
        .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(gateway)
}

async fn models(State(gateway): State<Arc<Gateway>>) -> Json<Value> {
    let data = gateway
        .config
        .model
        .as_ref()
        .map(|model| json!({"id":model,"object":"model","created":0,"owned_by":"antigravity"}))
        .into_iter()
        .collect::<Vec<_>>();
    Json(json!({"object":"list","data":data}))
}

async fn responses(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if headers.contains_key("origin") {
        return error(StatusCode::FORBIDDEN, "browser origins are not allowed");
    }
    let model = match crate::protocol::required_string(&request, "model") {
        Ok(model) => model.to_owned(),
        Err(_) => return error(StatusCode::BAD_REQUEST, "model is required"),
    };
    let stream = match request.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(stream)) => *stream,
        _ => return error(StatusCode::BAD_REQUEST, "stream must be boolean"),
    };
    let translated = translate(&request, &mut *gateway.replay.lock().await);
    let mut body = match translated {
        Ok(body) => body,
        Err(error_value) => return error(StatusCode::BAD_REQUEST, &error_value.to_string()),
    };
    let registry = match tools(&request) {
        Ok((_, registry)) => registry,
        Err(error_value) => return error(StatusCode::BAD_REQUEST, &error_value.to_string()),
    };
    let session = headers
        .get("session_id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("default");
    body["sessionId"] = json!(
        session
            .as_bytes()
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| (hash
                ^ u64::from(*byte))
            .wrapping_mul(0x100000001b3))
            .to_string()
    );
    let upstream = match upstream(&gateway, &model, body).await {
        Ok(response) => response,
        Err(error_value) => {
            eprintln!("request model={model}: {error_value:#}");
            let status = error_value
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status)
                .filter(|status| matches!(status.as_u16(), 401 | 403 | 429))
                .unwrap_or(StatusCode::BAD_GATEWAY);
            return error(status, "Antigravity request failed; see gateway stderr");
        }
    };
    let translator = Translator::new(&model, registry, &gateway.config.reasoning);
    let events = translate_stream(gateway, upstream, translator);
    if stream {
        Sse::new(events.map(|event| {
            event.map(|data| {
                Event::default()
                    .event(data["type"].as_str().unwrap_or("error"))
                    .data(data.to_string())
            })
        }))
        .keep_alive(KeepAlive::default())
        .into_response()
    } else {
        futures::pin_mut!(events);
        while let Some(Ok(value)) = events.next().await {
            if value["type"] == "response.completed" || value["type"] == "response.incomplete" {
                return Json(value["response"].clone()).into_response();
            }
            if value["type"] == "response.failed" {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Antigravity stream failed; see gateway stderr",
                );
            }
        }
        error(StatusCode::BAD_GATEWAY, "stream ended without completion")
    }
}

async fn upstream(gateway: &Gateway, model: &str, request: Value) -> Result<reqwest::Response> {
    let mut token =
        auth::access_token(&gateway.config, &gateway.client, &gateway.credentials).await?;
    let project = gateway.project(&token).await?;
    let url = format!(
        "{}/v1internal:streamGenerateContent?alt=sse",
        gateway.config.base_url.trim_end_matches('/')
    );
    let body = json!({"project":project,"model":model,"request":request,"requestType":"agent","userAgent":"antigravity","requestId":format!("agent/{}",uuid::Uuid::new_v4())});
    let mut response = None;
    for attempt in 0..2 {
        let sent = gateway
            .client
            .post(&url)
            .bearer_auth(&token)
            .header("User-Agent", &gateway.config.user_agent)
            .header(
                "client-metadata",
                "ideType=ANTIGRAVITY,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI",
            )
            .json(&body)
            .send()
            .await?;
        if attempt == 0
            && sent.status() == StatusCode::UNAUTHORIZED
            && gateway.config.access_token.is_none()
        {
            auth::invalidate_rejected_token(&gateway.credentials, &token).await;
            token =
                auth::access_token(&gateway.config, &gateway.client, &gateway.credentials).await?;
            continue;
        }
        response = Some(sent.error_for_status()?);
        break;
    }
    let response = response.context("backend authentication retry exhausted")?;
    ensure!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream")),
        "backend did not return SSE"
    );
    Ok(response)
}

fn translate_stream(
    gateway: Arc<Gateway>,
    upstream: reqwest::Response,
    mut translator: Translator,
) -> impl futures::Stream<Item = std::result::Result<Value, Infallible>> + Send {
    async_stream::stream! {
        yield Ok(translator.created());
        let mut bytes = 0_usize;
        let limit = gateway.config.response_bytes;
        let chunks = upstream.bytes_stream().map(move |chunk| -> Result<_> {
            let chunk = chunk.context("backend stream read failed")?;
            bytes = bytes.saturating_add(chunk.len());
            ensure!(bytes <= limit, "backend response exceeds byte limit");
            Ok(chunk)
        });
        let events = chunks.eventsource();
        futures::pin_mut!(events);
        let mut failed = false;
        while let Some(event) = events.next().await {
            let translated = async {
                let event = event.context("invalid backend SSE")?;
                if event.data == "[DONE]" { return Ok(Vec::new()); }
                let value: Value = serde_json::from_str(&event.data).context("invalid backend SSE JSON")?;
                translator.ingest(&value, &mut *gateway.replay.lock().await)
            }.await;
            match translated {
                Ok(events) => { for event in events { yield Ok(event); } }
                Err(error_value) => { eprintln!("stream request_id={}: {error_value:#}",translator.response["id"]); failed = true; break; }
            }
        }
        if failed { yield Ok(translator.failed()); }
        else {
            match translator.completed() {
                Ok(event) => yield Ok(event),
                Err(error_value) => { eprintln!("stream request_id={}: {error_value:#}",translator.response["id"]); yield Ok(translator.failed()); }
            }
        }
    }
}

pub fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":{"type":if status.is_client_error() {"invalid_request_error"} else {"server_error"},"message":message,"code":null,"param":null}}))).into_response()
}
