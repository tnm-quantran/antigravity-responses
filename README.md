# antigravity-responses

Standalone Rust gateway for Codex's HTTP Responses API and Antigravity's
Cloud Code Assist streaming protocol. No dependency on Codex crates.
Codex executes tools and owns its sandbox/approvals. RTK and Ponytail remain
Codex integrations; the gateway neither executes tools nor compresses output.

## Run

```sh
cargo install --locked --path ./antigravity-responses
export GOOGLE_ANTIGRAVITY_CLIENT_ID='<your OAuth client id>'
export GOOGLE_ANTIGRAVITY_CLIENT_SECRET='<your OAuth client secret if required>'
export ANTIGRAVITY_MODEL='<actual Antigravity wire model slug>'
export ANTIGRAVITY_CREDENTIALS='<absolute private credential file path>'
antigravity-responses login
antigravity-responses setup codex
antigravity-responses serve
# In another terminal:
codex -p antigravity_responses
```

Use the same environment for setup, login and serve. `login` prints a browser
URL and waits for a loopback callback; PKCE S256 and callback state are required.
Use `login --callback-port <port>` if your registered OAuth client requires a
specific loopback port. Credentials are separate from Codex authentication.
Access tokens refresh before expiry and once after an upstream 401.
For an externally managed token, set `ANTIGRAVITY_ACCESS_TOKEN`; that mode has
no gateway token refresh. Do not put secrets in command-line arguments.

The account must already have a Cloud Code Assist project. The gateway reads
`loadCodeAssist`, or accepts `ANTIGRAVITY_PROJECT`. It does not onboard an account.
Model slugs pass through unchanged: use the backend wire slug, including any
required suffix. `/v1/models` lists the configured model, not a remote catalog.
Configure `ANTIGRAVITY_USER_AGENT` if the backend requires a specific client
identity; the default is `antigravity`. Real account/header/schema compatibility
is not yet verified.

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

`setup codex` adds `model_providers.antigravity_responses` and
`profiles.antigravity_responses`. It preserves the default provider and unrelated
settings, backs up existing config before writes, rejects conflicting gateway
fields, and is idempotent. Select an alternative home with `setup codex
--codex-home <directory>` or `CODEX_HOME`. The different id avoids overwriting
the fork's native Antigravity provider. WebSocket support is disabled explicitly.

RTK setup invokes `rtk init -g --codex --no-patch --no-trust-filters`. Ponytail
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

Supported: text and inline images; developer/system instructions; function,
namespace, custom/freeform and local tool-search calls; raw tool outputs;
reasoning summary SSE; full-history multi-turn; non-streaming JSON and streaming
SSE with ordered sequence numbers; token usage; incomplete and failed responses.
The `raw-thought` mode displays provider thought text only through reasoning;
`hidden` retains replay parts without displaying thought text.

Signatures remain opaque. Original parts are cached and replayed in order;
provider call ids are kept when present. No fabricated/dummy signature fallback.
Replay is **in memory**: default 24-hour TTL, 64 MiB, 4096 entries. Request and
upstream response limits default to 16 MiB each. Exceeding a limit or losing
required replay state produces an error. Restarting the gateway requires a new
conversation; persistent resume is not implemented.

Not supported: `previous_response_id`, WebSockets, remote compaction, hosted
OpenAI tools, remote image fetches, audio/video, named/forced tool-choice objects,
account onboarding, remote model discovery or every JSON Schema keyword.
Unsupported tools, content, formats and schemas are rejected rather than
silently dropped. Unknown auxiliary Responses metadata is ignored.
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
