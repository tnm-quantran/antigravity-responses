use crate::config::Config;
use anyhow::{Context, Result, ensure};
use axum::{Router, extract::Query, routing::get};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Default)]
pub struct TokenCache {
    credentials: Option<Credentials>,
    pending_save: bool,
}

pub async fn invalidate_rejected_token(cache: &Mutex<TokenCache>, rejected: &str) {
    let mut cache = cache.lock().await;
    if let Some(credentials) = cache.credentials.as_mut()
        && credentials.access_token == rejected
    {
        credentials.expires_at = 0;
    }
}

pub fn save_credentials(path: &Path, credentials: &Credentials) -> Result<()> {
    crate::storage::write_private(path, &serde_json::to_vec(credentials)?)
}

pub fn read_credentials(path: &Path) -> Result<Credentials> {
    serde_json::from_slice(&std::fs::read(path).context("credentials missing; run login")?)
        .context("invalid credential file")
}

pub async fn access_token(
    config: &Config,
    client: &reqwest::Client,
    cache: &Mutex<TokenCache>,
) -> Result<String> {
    if let Some(token) = &config.access_token {
        ensure!(!token.is_empty(), "empty ANTIGRAVITY_ACCESS_TOKEN");
        return Ok(token.clone());
    }
    // ponytail: serialize token refresh for this single-account gateway; split per account if needed.
    let mut cache = cache.lock().await;
    if cache.credentials.is_none() {
        cache.credentials = Some(read_credentials(&config.credentials)?);
    }
    if cache.pending_save {
        save_credentials(
            &config.credentials,
            cache
                .credentials
                .as_ref()
                .context("missing pending credentials")?,
        )?;
        cache.pending_save = false;
    }
    let credentials = cache.credentials.as_ref().context("missing credentials")?;
    if credentials.expires_at > now() + 60 {
        return Ok(credentials.access_token.clone());
    }
    let client_id = config
        .client_id
        .clone()
        .context("set GOOGLE_ANTIGRAVITY_CLIENT_ID")?;
    let secret = config.client_secret.clone();
    let refresh = credentials
        .refresh_token
        .clone()
        .context("refresh token missing; run login")?;
    let mut form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("client_id", client_id),
        ("refresh_token", refresh.clone()),
    ];
    if let Some(secret) = secret {
        form.push(("client_secret", secret));
    }
    let credentials = grant(config, client, &form, Some(refresh)).await?;
    // Retain rotated credentials before attempting disk I/O, even when persistence fails.
    cache.credentials = Some(credentials.clone());
    cache.pending_save = true;
    save_credentials(&config.credentials, &credentials)?;
    cache.pending_save = false;
    Ok(credentials.access_token)
}

async fn grant(
    config: &Config,
    client: &reqwest::Client,
    form: &[(&str, String)],
    refresh: Option<String>,
) -> Result<Credentials> {
    let url = reqwest::Url::parse(&config.token_url)?;
    ensure!(
        url.scheme() == "https" || url.host_str() == Some("127.0.0.1"),
        "OAuth token endpoint requires HTTPS"
    );
    let response = client
        .post(url)
        .form(form)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "OAuth token endpoint returned HTTP {}",
        response.status()
    );
    let value: serde_json::Value = response.json().await?;
    Ok(Credentials {
        access_token: value["access_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .context("OAuth response missing access token")?
            .to_owned(),
        refresh_token: value["refresh_token"]
            .as_str()
            .map(str::to_owned)
            .or(refresh),
        expires_at: now()
            + value["expires_in"]
                .as_u64()
                .context("OAuth response missing expires_in")?,
    })
}

pub fn authorization_url(
    client_id: &str,
    redirect: &str,
    state: &str,
    verifier: &str,
) -> Result<reqwest::Url> {
    let endpoint = std::env::var("ANTIGRAVITY_AUTHORIZATION_URL")
        .unwrap_or_else(|_| "https://accounts.google.com/o/oauth2/v2/auth".into());
    let mut url = reqwest::Url::parse(&endpoint)?;
    ensure!(
        url.scheme() == "https",
        "authorization endpoint requires HTTPS"
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    url.query_pairs_mut().extend_pairs([
        ("client_id",client_id), ("redirect_uri",redirect), ("response_type","code"), ("state",state),
        ("code_challenge",challenge.as_str()), ("code_challenge_method","S256"),
        ("access_type","offline"), ("prompt","consent"),
        ("scope","https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile"),
    ]);
    Ok(url)
}

pub async fn login(config: &Config, callback_port: u16) -> Result<()> {
    let client_id = config
        .client_id
        .clone()
        .context("set GOOGLE_ANTIGRAVITY_CLIENT_ID")?;
    let secret = config.client_secret.clone();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", callback_port)).await?;
    let redirect = format!("http://{}/oauth-callback", listener.local_addr()?);
    let state = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let (sender, receiver) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(sender)));
    let expected = state.clone();
    let app = Router::new().route(
        "/oauth-callback",
        get(move |Query(query): Query<HashMap<String, String>>| {
            let sender = sender.clone();
            let expected = expected.clone();
            async move {
                if query.get("state") != Some(&expected) {
                    return (axum::http::StatusCode::BAD_REQUEST, "Invalid OAuth state");
                }
                if let Some(sender) = sender.lock().await.take() {
                    let _ = sender.send(query);
                }
                (
                    axum::http::StatusCode::OK,
                    "Callback received. Return to the terminal.",
                )
            }
        }),
    );
    let url = authorization_url(&client_id, &redirect, &state, &verifier)?;
    println!("Open this URL in your browser:\n{url}");
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let callback =
        tokio::time::timeout(Duration::from_secs(config.timeout_seconds), receiver).await;
    server.abort();
    let query = callback.context("OAuth login timed out")??;
    ensure!(!query.contains_key("error"), "OAuth authorization denied");
    let code = query.get("code").context("OAuth callback missing code")?;
    let mut form = vec![
        ("grant_type", "authorization_code".into()),
        ("client_id", client_id),
        ("code", code.clone()),
        ("redirect_uri", redirect),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = secret {
        form.push(("client_secret", secret));
    }
    let credentials = grant(config, &config.client()?, &form, None).await?;
    ensure!(
        credentials.refresh_token.is_some(),
        "OAuth did not return a refresh token; repeat login with consent"
    );
    save_credentials(&config.credentials, &credentials)?;
    println!("Credentials saved to {}", config.credentials.display());
    Ok(())
}
