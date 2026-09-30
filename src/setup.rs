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
    let model = config
        .model
        .as_deref()
        .context("set --model or ANTIGRAVITY_MODEL before setup")?;
    let provider = format!("http://{}/v1", config.listen);
    let fields = [
        ("model_providers", "name", "Antigravity Responses"),
        ("model_providers", "base_url", provider.as_str()),
        ("model_providers", "wire_api", "responses"),
        ("profiles", "model_provider", "antigravity_responses"),
        ("profiles", "model", model),
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
    let result = document.to_string();
    let _: DocumentMut = result.parse()?;
    Ok(result)
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
        let path = home.join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let merged = merge_codex(&text, config)?;
        if merged != text {
            std::fs::create_dir_all(home)?;
            if path.exists() {
                let backup = home.join(format!("config.toml.{}.bak", uuid::Uuid::new_v4()));
                std::fs::copy(&path, &backup)?;
                println!("Backup: {}", backup.display());
            }
            crate::storage::write_private(&path, merged.as_bytes())?;
            ensure!(
                std::fs::read_to_string(&path)? == merged,
                "config verification failed"
            );
        }
        println!("Codex profile ready: codex -p antigravity_responses");
    }
    if matches!(target, "rtk" | "all") {
        run(
            "rtk",
            &["init", "-g", "--codex", "--no-patch", "--no-trust-filters"],
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
        println!(
            "Review/trust Ponytail hooks in Codex. Select its mode inside Codex; gateway does not configure behavior."
        );
    }
    Ok(())
}

fn run(program: &str, args: &[&str], home: &Path) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .env("CODEX_HOME", home)
        .status()
        .with_context(|| format!("run {program}"))?;
    ensure!(status.success(), "{program} exited with {status}");
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
        std::fs::read_to_string(home.join("config.toml"))
            .context("missing Codex config")
            .and_then(|text| {
                let document: DocumentMut = text.parse()?;
                ensure!(
                    document
                        .get("profiles")
                        .and_then(|profiles| profiles.get("antigravity_responses"))
                        .and_then(|profile| profile.get("model_provider"))
                        .and_then(Item::as_str)
                        == Some("antigravity_responses"),
                    "profile missing"
                );
                ensure!(
                    document
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
