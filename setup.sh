#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"
started=$SECONDS

command -v cargo >/dev/null 2>&1 || {
  echo "Rust/Cargo chưa được cài. Cài tại https://rustup.rs/" >&2
  exit 1
}
command -v systemctl >/dev/null 2>&1 || {
  echo "Không tìm thấy systemd/systemctl." >&2
  exit 1
}
systemctl --user show-environment >/dev/null 2>&1 || {
  echo "systemd user manager chưa sẵn sàng trong phiên này." >&2
  exit 1
}
if [[ -n "${ANTIGRAVITY_ACCESS_TOKEN:-}" ]]; then
  echo "Hãy bỏ ANTIGRAVITY_ACCESS_TOKEN để daemon dùng OAuth có refresh token." >&2
  exit 1
fi

cargo install --locked --path . --force
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
gateway="${CARGO_HOME:-$HOME/.cargo}/bin/antigravity-responses"
test -x "$gateway"

credentials="${ANTIGRAVITY_CREDENTIALS:-antigravity.credentials.json}"
if [[ ! -s "$credentials" ]]; then
  "$gateway" login
fi

"$gateway" setup codex >/dev/null
if ! command -v rtk >/dev/null 2>&1; then
  cargo install --git https://github.com/rtk-ai/rtk --branch master rtk
fi
"$gateway" setup rtk >/dev/null
if command -v node >/dev/null 2>&1 && command -v codex >/dev/null 2>&1; then
  "$gateway" setup ponytail >/dev/null
fi

unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
mkdir -p "$unit_dir"
unit_escape() {
  local value="$1"
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//%/%%}
  printf '"%s"' "$value"
}
unit_path_escape() {
  local value="$1"
  value=${value//\\/\\\\}
  value=${value// /\\x20}
  value=${value//$'\t'/\\x09}
  value=${value//%/%%}
  printf '%s' "$value"
}
cat > "$unit_dir/antigravity-responses.service" <<EOF
[Unit]
Description=Antigravity Responses API Gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$(unit_path_escape "$PWD")
ExecStart=$(unit_escape "$gateway") serve
Restart=on-failure
RestartSec=5
EOF
if [[ -n "${ANTIGRAVITY_CREDENTIALS:-}" ]]; then
  printf 'Environment=ANTIGRAVITY_CREDENTIALS=%s\n' \
    "$(unit_escape "$ANTIGRAVITY_CREDENTIALS")" >> "$unit_dir/antigravity-responses.service"
fi
cat >> "$unit_dir/antigravity-responses.service" <<EOF

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload >/dev/null
systemctl --user enable antigravity-responses.service >/dev/null
systemctl --user restart antigravity-responses.service >/dev/null
elapsed=$((SECONDS - started))
if ((elapsed >= 60)); then
  printf 'Done in %dm %02ds\n' "$((elapsed / 60))" "$((elapsed % 60))"
else
  printf 'Done in %ds\n' "$elapsed"
fi
