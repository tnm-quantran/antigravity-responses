use antigravity_responses::{
    auth::{Credentials, TokenCache, access_token, read_credentials, save_credentials},
    config::Config,
    setup,
};
use axum::{Router, extract::Form, routing::post};
use clap::Parser;
use pretty_assertions::assert_eq;
use std::{
    collections::HashMap,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Mutex;

#[derive(Parser)]
struct Options {
    #[command(flatten)]
    config: Config,
}

#[tokio::test]
async fn concurrent_refresh_runs_once_and_persists_rotated_refresh_token() {
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route("/token",post({
        let count = count.clone();
        move |Form(form): Form<HashMap<String,String>>| {
            let count = count.clone();
            async move {
                assert_eq!(form["grant_type"],"refresh_token");
                assert_eq!(form["refresh_token"],"old-refresh");
                assert!(!form["client_secret"].is_empty());
                count.fetch_add(1,Ordering::SeqCst);
                axum::Json(serde_json::json!({"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://{}/token", listener.local_addr().unwrap());
    let task = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    let path = std::env::temp_dir().join(format!("ag-refresh-{}.json", uuid::Uuid::new_v4()));
    save_credentials(
        &path,
        &Credentials {
            access_token: "expired".into(),
            refresh_token: Some("old-refresh".into()),
            expires_at: 0,
        },
    )
    .unwrap();
    let mut config = Options::parse_from([
        "test",
        "--token-url",
        &token_url,
        "--credentials",
        path.to_str().unwrap(),
    ])
    .config;
    config.access_token = None;
    let client = config.client().unwrap();
    let cache = Mutex::new(TokenCache::default());
    let (first, second) = tokio::join!(
        access_token(&config, &client, &cache),
        access_token(&config, &client, &cache)
    );
    assert_eq!(
        (first.unwrap(), second.unwrap()),
        ("new-access".into(), "new-access".into())
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        read_credentials(&path).unwrap().refresh_token,
        Some("new-refresh".into())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    task.abort();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn dotenv_configures_model_credentials_and_oauth_for_standalone_cli() {
    let root = std::env::temp_dir().join(format!("ag-dotenv-{}", uuid::Uuid::new_v4()));
    let codex_home = root.join("codex");
    std::fs::create_dir_all(&root).unwrap();
    let credentials = root.join("credentials.json");
    save_credentials(
        &credentials,
        &Credentials {
            access_token: "test-access".into(),
            refresh_token: None,
            expires_at: u64::MAX,
        },
    )
    .unwrap();
    std::fs::write(
        root.join(".env"),
        format!(
            "ANTIGRAVITY_MODEL=dotenv-model\nANTIGRAVITY_CREDENTIALS={}\nCODEX_HOME=/ignored\n",
            credentials.display()
        ),
    )
    .unwrap();

    let setup = Command::new(env!("CARGO_BIN_EXE_antigravity-responses"))
        .current_dir(&root)
        .env_clear()
        .env("CODEX_HOME", &codex_home)
        .args(["setup", "codex"])
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let config = std::fs::read_to_string(codex_home.join("config.toml")).unwrap();
    assert!(config.contains("[model_providers.antigravity_responses]"));
    assert!(config.contains("base_url = \"http://127.0.0.1:8787/v1\""));
    assert!(config.contains("wire_api = \"responses\""));
    assert!(config.contains("requires_openai_auth = false"));
    assert!(!config.contains("[profiles.antigravity_responses]"));
    assert!(!config.contains("access_token"));
    let profile =
        std::fs::read_to_string(codex_home.join("antigravity_responses.config.toml")).unwrap();
    assert!(profile.contains("model = \"dotenv-model\""));
    assert!(profile.contains("model_provider = \"antigravity_responses\""));
    assert!(profile.contains("web_search = \"live\""));

    let doctor = Command::new(env!("CARGO_BIN_EXE_antigravity-responses"))
        .current_dir(&root)
        .env_clear()
        .env("CODEX_HOME", &codex_home)
        .arg("doctor")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("OK OAuth credentials"));

    let occupied_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied_port.local_addr().unwrap().port().to_string();
    let login = Command::new(env!("CARGO_BIN_EXE_antigravity-responses"))
        .current_dir(&root)
        .env_clear()
        .env("CODEX_HOME", codex_home)
        .args(["login", "--callback-port", &port])
        .output()
        .unwrap();
    let login_error = String::from_utf8_lossy(&login.stderr).to_ascii_lowercase();
    assert!(
        login_error.contains("address already in use"),
        "{login_error}"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn setup_updates_existing_web_search_mode_to_live() {
    let config = Options::parse_from(["test", "--model", "gemini-test"]).config;
    for mode in ["cached", "disabled", "live"] {
        let profile = setup::merge_codex_profile(
            &format!("web_search = \"{mode}\"\n"),
            &config,
            std::path::Path::new("models.json"),
        )
        .unwrap();
        assert!(profile.contains("web_search = \"live\""));
        assert_eq!(
            setup::merge_codex_profile(&profile, &config, std::path::Path::new("models.json"))
                .unwrap(),
            profile
        );
    }
}

#[test]
fn setup_backs_up_existing_config_and_repeated_setup_has_no_extra_backup() {
    let home = std::env::temp_dir().join(format!("ag-config-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    let original = "# user's settings\nmodel = 'existing'\nprofile = 'antigravity_responses'\n[profiles.antigravity_responses]\nmodel = 'legacy'\nmodel_provider = 'antigravity_responses'\n[profiles.user]\nmodel = 'mine'\n";
    std::fs::write(home.join("config.toml"), original).unwrap();
    let config = Options::parse_from(["test", "--model", "gemini-test"]).config;
    setup::setup(&config, "codex", &home).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(home.join("config.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let backups: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "bak"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), original);
    let migrated = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(!migrated.contains("profile = 'antigravity_responses'"));
    assert!(!migrated.contains("[profiles.antigravity_responses]"));
    assert!(migrated.contains("[profiles.user]"));
    assert!(home.join("antigravity_responses.config.toml").exists());
    setup::setup(&config, "codex", &home).unwrap();
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 4);
    for entry in std::fs::read_dir(&home).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    std::fs::remove_dir(home).unwrap();
}
