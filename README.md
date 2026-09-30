# antigravity-responses

Standalone Rust gateway exposing an OpenAI Responses API compatible HTTP
interface over Antigravity Cloud Code Assist. Serving requests does not require
Codex, its config, or Codex crates. Codex, RTK and Ponytail are optional client
integrations; the gateway neither executes tools nor compresses output.

## Run

To install the CLI, configure Codex, sign in with Google and install/start the
gateway as a systemd user service, run `./setup.sh`. Approving Google OAuth in
the browser is still required. The script expects Cargo and an active systemd
user manager; it uses `.env` when present.

Check the daemon with `systemctl --user status antigravity-responses` and follow
its logs with `journalctl --user -u antigravity-responses -f`. It starts with
your user session. To start it at boot before logging in, enable lingering with
`loginctl enable-linger "$USER"` if your system permits it.

```sh
cargo install --locked --path .
antigravity-responses login
antigravity-responses setup codex
antigravity-responses serve
antigravity-responses usage
```

The default model is `gemini-3.8-flash-medium`; you can override it with
`--model` or `ANTIGRAVITY_MODEL`. The Antigravity OAuth client
credentials are built into the app. An optional credentials file path and other
settings can come from `.env`; already-exported environment variables take
precedence. Start with `.env.example` and keep `.env` private.

Call the Responses endpoint directly with any compatible client:

```sh
curl http://127.0.0.1:8787/v1/responses \
  -H 'content-type: application/json' \
  -d '{"model":"<selected-model-slug>","input":"Hello"}'
```

`login` authenticates your Google account, prints a browser URL and waits for a
loopback callback; PKCE S256 and callback state are required. `setup codex`
writes the default model into a Codex profile that points to the local gateway.
Start the gateway, then select it with
`codex -p antigravity_responses`. OAuth credentials belong to the gateway and
are not stored in Codex's auth config.
Use `login --callback-port <port>` if your registered OAuth client requires a
specific loopback port. Access tokens refresh before expiry and once after an
upstream 401.
For an externally managed token, set `ANTIGRAVITY_ACCESS_TOKEN`; that mode has
no gateway token refresh. Do not put secrets in command-line arguments.

The gateway reads the account's Cloud Code Assist project with `loadCodeAssist`
and onboards the account to the free tier when needed. Set `ANTIGRAVITY_PROJECT`
to provide a project id directly.
Model slugs pass through unchanged: use the backend wire slug, including any
required suffix. `/v1/models` lists the configured model, not a remote catalog.
Configure `ANTIGRAVITY_USER_AGENT` to override the default Antigravity CLI
identity. Real account/schema compatibility still requires a live authenticated
request.

Settings are shown by `antigravity-responses --help`. Global options go before
the subcommand. The server binds only to `127.0.0.1`. Backend and token endpoints
require HTTPS, except loopback HTTP for tests. Browser-origin requests are rejected.

## Setup and diagnostics

```sh
antigravity-responses setup rtk
antigravity-responses setup ponytail
antigravity-responses setup all
antigravity-responses doctor
```

`setup codex` adds `model_providers.antigravity_responses` to `config.toml` and
writes the default model and provider to
`antigravity_responses.config.toml`, plus a profile-scoped model catalog for the
Codex model picker. It removes the matching legacy profile and
selector from `config.toml`, preserves unrelated settings, overwrites the
gateway-owned profile fields, backs up changed files, and is idempotent. Select an alternative home with `setup codex
--codex-home <directory>` or `CODEX_HOME`. The different id avoids overwriting
the fork's native Antigravity provider. WebSocket support is disabled explicitly.
The Codex model catalog contains Gemini 3.6, 3.7 and 3.8 models, ordered by
low, medium and high reasoning effort within each generation.

RTK setup invokes `rtk init -g --codex --no-trust-filters`. Ponytail
setup invokes the official Codex marketplace/plugin commands. Missing commands
or failed setup return an error. `setup all` may have completed earlier steps
before a later integration fails; backups and command output identify those
changes. Review/trust hooks and choose Ponytail's mode in Codex. These commands
do not install missing executables or bypass plugin trust.

`doctor` checks config, credential file readability, provider URL/profile,
gateway health and executable availability. It does not claim live inference,
RTK interception, Ponytail activation, sandbox behavior or protocol conformance
based on those checks. Start `serve` before running doctor.

## Protocol coverage and limits

Supported Responses subset: text and inline images; developer/system instructions; function,
namespace, custom/freeform and local tool-search calls; raw tool outputs;
reasoning summary SSE; full-history and `previous_response_id` multi-turn; non-streaming JSON and streaming
SSE with ordered sequence numbers; token usage; incomplete and failed responses.
The `raw-thought` mode displays provider thought text only through reasoning;
`hidden` retains replay parts without displaying thought text.

Signatures remain opaque. Original parts are cached and replayed in order;
provider call ids are kept when present. No fabricated/dummy signature fallback.
Replay is persisted beside the credentials file with private permissions and
restored after restart. Default TTL is 24 hours, with 64 MiB and 4096 entries.
Request and upstream response limits default to 16 MiB each. Exceeding a limit
or losing required replay state produces an error.

This is not full OpenAI API parity: unsupported fields/features are rejected or
unimplemented when Antigravity has no equivalent. `web_search` and
`web_search_preview` use native Google Search grounding through separate sidecar
requests, so they can be used alongside function tools. A Responses turn runs
at most three web searches; an upstream search failure fails the response.
Not supported: response retrieval/deletion endpoints, WebSockets,
remote compaction, other hosted OpenAI tools, remote image fetches, audio/video,
remote model discovery or every JSON Schema keyword. Named function, required,
and `allowed_tools` choices are translated to Antigravity function calling.
Unsupported tool schema constraints are reported and omitted by default;
`ANTIGRAVITY_SCHEMA_POLICY=reject-lossy` rejects schemas when conversion would
drop constraints. Unknown auxiliary Responses metadata is ignored.
The gateway transports `apply_patch` input; Codex performs the patch itself.

Credential/config files are written using private temporary files and atomic
rename (0600 on Unix). On Windows, choose a directory with a private inherited
ACL. Token refresh is serialized for one account. Changing account requires
restarting the gateway. Logs omit token values and backend error bodies.

## Verify

From `codex-rs/` in this checkout:

```sh
just test --manifest-path ../antigravity-responses/Cargo.toml --offline
just fix --manifest-path ../antigravity-responses/Cargo.toml -p antigravity-responses
cargo fmt --manifest-path ../antigravity-responses/Cargo.toml
```

The tests use local fake HTTP backends and fake credentials. They cover early
SSE delivery, truncated streams, tool/signature round trips, reasoning channel
separation, schema refs, PKCE, refresh concurrency, private writes and idempotent
config backups. They require permission to bind loopback ports. Live Codex +
Antigravity, RTK/Ponytail behavior and Windows/macOS are separate acceptance
checks, still pending. The existing Codex Antigravity adapter remains intact.

Protocol references: [Responses streaming](https://developers.openai.com/api/docs/guides/streaming-responses)
and [Gemini thinking](https://ai.google.dev/gemini-api/docs/thinking).
