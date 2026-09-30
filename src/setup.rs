use crate::{auth, config::Config};
use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};
use std::process::Command;
use toml_edit::{DocumentMut, Item, Table, value};

pub fn codex_home(path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("set --codex-home or CODEX_HOME")?;
    Ok(PathBuf::from(home).join(".codex"))
}

pub fn merge_codex(text: &str, config: &Config) -> Result<String> {
    let mut document: DocumentMut = text.parse().context("invalid Codex config TOML")?;
    let provider = format!("http://{}/v1", config.listen);
    let fields = [
        ("model_providers", "name", "Antigravity Responses"),
        ("model_providers", "base_url", provider.as_str()),
        ("model_providers", "wire_api", "responses"),
    ];
    for (group, field, desired) in fields {
        ensure_table(&mut document[group])?;
        ensure_table(&mut document[group]["antigravity_responses"])?;
        let current = document[group]["antigravity_responses"].get(field);
        ensure!(
            current.is_none_or(|current| current.as_str() == Some(desired)),
            "conflicting {group}.antigravity_responses.{field}; edit or remove that entry explicitly"
        );
        document[group]["antigravity_responses"][field] = value(desired);
    }
    let provider = &mut document["model_providers"]["antigravity_responses"];
    ensure!(
        provider
            .get("requires_openai_auth")
            .is_none_or(|current| current.as_bool() == Some(false)),
        "conflicting requires_openai_auth"
    );
    ensure!(
        provider
            .get("supports_websockets")
            .is_none_or(|current| current.as_bool() == Some(false)),
        "conflicting supports_websockets"
    );
    provider["requires_openai_auth"] = value(false);
    provider["supports_websockets"] = value(false);
    if let Some(profiles) = document.get_mut("profiles") {
        ensure!(profiles.is_table(), "profiles section must be a TOML table");
        profiles
            .as_table_mut()
            .unwrap()
            .remove("antigravity_responses");
        if profiles.as_table().unwrap().is_empty() {
            document.as_table_mut().remove("profiles");
        }
    }
    if document.get("profile").and_then(Item::as_str) == Some("antigravity_responses") {
        document.as_table_mut().remove("profile");
    }
    let result = document.to_string();
    let _: DocumentMut = result.parse()?;
    Ok(result)
}

pub fn merge_codex_profile(text: &str, config: &Config, model_catalog: &Path) -> Result<String> {
    let mut document: DocumentMut = text.parse().context("invalid Codex profile TOML")?;
    document.as_table_mut().remove("show_raw_agent_reasoning");
    let model = config
        .model
        .as_deref()
        .context("set --model or ANTIGRAVITY_MODEL before setup")?;
    for (field, desired) in [
        ("model_provider", "antigravity_responses"),
        ("model", model),
        ("web_search", "live"),
        (
            "model_reasoning_effort",
            crate::protocol::model_reasoning_effort(model).unwrap_or("medium"),
        ),
    ] {
        document[field] = value(desired);
    }
    document["model_catalog_json"] = value(model_catalog.to_string_lossy().as_ref());
    ensure_table(&mut document["sandbox_workspace_write"])?;
    let sandbox = &mut document["sandbox_workspace_write"];
    ensure!(
        sandbox
            .get("network_access")
            .is_none_or(|current| current.as_bool() == Some(true)),
        "conflicting sandbox_workspace_write.network_access"
    );
    sandbox["network_access"] = value(true);

    ensure_table(&mut document["features"])?;
    ensure_table(&mut document["features"]["network_proxy"])?;
    let proxy = &mut document["features"]["network_proxy"];
    ensure!(
        proxy
            .get("enabled")
            .is_none_or(|current| current.as_bool() == Some(true)),
        "conflicting features.network_proxy.enabled"
    );
    proxy["enabled"] = value(true);
    ensure_table(&mut proxy["domains"])?;
    for domain in ["api.github.com", "api.clickup.com"] {
        ensure!(
            proxy["domains"]
                .get(domain)
                .is_none_or(|current| current.as_str() == Some("allow")),
            "conflicting features.network_proxy.domains.{domain}"
        );
        proxy["domains"][domain] = value("allow");
    }
    Ok(document.to_string())
}

fn ensure_table(item: &mut Item) -> Result<()> {
    if item.is_none() {
        *item = Item::Table(Table::new());
    }
    ensure!(item.is_table(), "config section must be a TOML table");
    Ok(())
}

pub fn setup(config: &Config, target: &str, home: &Path) -> Result<()> {
    if matches!(target, "codex" | "all") {
        let model_catalog = home.join("antigravity_responses.models.json");
        let catalog = crate::server::codex_model_catalog();
        let writes = [
            (
                model_catalog.clone(),
                serde_json::to_string_pretty(&catalog).context("serialize model catalog")?,
            ),
            (
                home.join("config.toml"),
                merge_codex(&read_config(&home.join("config.toml"))?, config)?,
            ),
            (
                home.join("antigravity_responses.config.toml"),
                merge_codex_profile(
                    &read_config(&home.join("antigravity_responses.config.toml"))?,
                    config,
                    &model_catalog,
                )?,
            ),
        ];
        for (path, merged) in writes {
            write_config(&path, &merged)?;
        }
    }
    if matches!(target, "rtk" | "all") {
        run(
            "rtk",
            &["init", "-g", "--codex", "--no-trust-filters"],
            home,
        )
        .context("install RTK and retry setup rtk; gateway works without RTK")?;
    }
    if matches!(target, "ponytail" | "all") {
        run("node", &["--version"], home).context("install Node.js before Ponytail setup")?;
        run(
            "codex",
            &["plugin", "marketplace", "add", "DietrichGebert/ponytail"],
            home,
        )?;
        run("codex", &["plugin", "add", "ponytail@ponytail"], home)?;
    }
    Ok(())
}

fn read_config(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn write_config(path: &Path, merged: &str) -> Result<()> {
    let text = read_config(path)?;
    if merged == text {
        return Ok(());
    }
    let home = path.parent().context("config path has no parent")?;
    std::fs::create_dir_all(home)?;
    if path.exists() {
        let filename = path.file_name().unwrap().to_string_lossy();
        let backup = home.join(format!("{filename}.{}.bak", uuid::Uuid::new_v4()));
        std::fs::copy(path, &backup)?;
    }
    crate::storage::write_private(path, merged.as_bytes())?;
    ensure!(
        std::fs::read_to_string(path)? == merged,
        "config verification failed"
    );
    Ok(())
}

fn run(program: &str, args: &[&str], home: &Path) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .env("CODEX_HOME", home)
        .output()
        .with_context(|| format!("run {program}"))?;
    ensure!(
        output.status.success(),
        "{program} exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

pub async fn doctor(config: &Config, home: &Path) -> Result<()> {
    let mut failures = Vec::new();
    check("configuration", config.validate(), &mut failures);
    check(
        "OAuth credentials",
        auth::read_credentials(&config.credentials).map(|_| ()),
        &mut failures,
    );
    check(
        "Codex config",
        std::fs::read_to_string(home.join("antigravity_responses.config.toml"))
            .context("missing Codex config")
            .and_then(|text| {
                let document: DocumentMut = text.parse()?;
                ensure!(
                    document.get("model_provider").and_then(Item::as_str)
                        == Some("antigravity_responses"),
                    "profile missing"
                );
                let provider_text = std::fs::read_to_string(home.join("config.toml"))?;
                let provider_config: DocumentMut = provider_text.parse()?;
                ensure!(
                    provider_config
                        .get("model_providers")
                        .and_then(|providers| providers.get("antigravity_responses"))
                        .and_then(|provider| provider.get("base_url"))
                        .and_then(Item::as_str)
                        == Some(format!("http://{}/v1", config.listen).as_str()),
                    "provider URL mismatch"
                );
                Ok(())
            }),
        &mut failures,
    );
    check(
        "gateway health",
        async {
            config
                .client()?
                .get(format!("http://{}/health", config.listen))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await?
                .error_for_status()?;
            Ok(())
        }
        .await,
        &mut failures,
    );
    for program in ["codex", "rtk", "node"] {
        check(
            program,
            Command::new(program)
                .arg("--version")
                .output()
                .context("executable missing")
                .and_then(|output| {
                    ensure!(output.status.success(), "version command failed");
                    Ok(())
                }),
            &mut failures,
        );
    }
    println!(
        "Protocol/live inference, RTK interception and Ponytail hook activation: not tested by doctor."
    );
    if !failures.is_empty() {
        bail!("doctor: {} check(s) failed", failures.len());
    }
    Ok(())
}

fn check(label: &str, result: Result<()>, failures: &mut Vec<String>) {
    match result {
        Ok(()) => println!("OK {label}"),
        Err(error) => {
            println!("FAIL {label}: {error:#}");
            failures.push(label.to_owned());
        }
    }
}
