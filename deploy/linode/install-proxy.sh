#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 1 ]; then
  echo "Usage: $0 <ssh-host> [public-hostname]" >&2
  echo "Example: $0 root@203.0.113.10 av-proxy.wavey.ai" >&2
  exit 1
fi

SSH_HOST="$1"
PUBLIC_HOSTNAME="${2:-av-proxy.wavey.ai}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
REMOTE_DIR="/opt/av-ingest-proxy/src"
REMOTE_PORT="${AV_INGEST_PROXY_REMOTE_PORT:-9444}"

ssh -o StrictHostKeyChecking=no "$SSH_HOST" "mkdir -p '$REMOTE_DIR'"

rsync -az --delete \
  --exclude target \
  --exclude node_modules \
  "$REPO_ROOT/Cargo.toml" \
  "$REPO_ROOT/Cargo.lock" \
  "$REPO_ROOT/crates" \
  "$SSH_HOST:$REMOTE_DIR/"

ssh -o StrictHostKeyChecking=no "$SSH_HOST" \
  "PUBLIC_HOSTNAME='$PUBLIC_HOSTNAME' REMOTE_PORT='$REMOTE_PORT' bash -s" <<'ENDSSH'
set -euo pipefail

install_build_deps() {
  if command -v pacman >/dev/null 2>&1; then
    pacman -Sy --needed --noconfirm base-devel ca-certificates caddy cmake curl openssl pkgconf
  elif command -v apt-get >/dev/null 2>&1; then
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y build-essential ca-certificates caddy cmake curl openssl pkg-config
  fi
}

install_rust() {
  if command -v cargo >/dev/null 2>&1; then
    return
  fi
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
}

install_build_deps
install_rust
if [ -f /root/.cargo/env ]; then
  # shellcheck disable=SC1091
  . /root/.cargo/env
fi

cd /opt/av-ingest-proxy/src
cargo build --release -p av-ingest-proxy
install -m 0755 target/release/av-ingest-proxy /usr/local/bin/av-ingest-proxy

id -u av-ingest-proxy >/dev/null 2>&1 || useradd --system --home-dir /var/lib/av-ingest-proxy --create-home --shell /usr/bin/nologin av-ingest-proxy
install -d -o av-ingest-proxy -g av-ingest-proxy -m 0750 /etc/av-ingest-proxy

if [ ! -f /etc/av-ingest-proxy/local.crt ] || [ ! -f /etc/av-ingest-proxy/local.key ]; then
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout /etc/av-ingest-proxy/local.key \
    -out /etc/av-ingest-proxy/local.crt \
    -days 3650 \
    -subj "/CN=av-ingest-proxy.local" >/dev/null 2>&1
  chown av-ingest-proxy:av-ingest-proxy /etc/av-ingest-proxy/local.crt /etc/av-ingest-proxy/local.key
  chmod 0640 /etc/av-ingest-proxy/local.crt /etc/av-ingest-proxy/local.key
fi

cat >/etc/systemd/system/av-ingest-proxy.service <<EOF
[Unit]
Description=av-ingest media range proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=av-ingest-proxy
Group=av-ingest-proxy
Environment=RUST_LOG=av_ingest_proxy=info,web_service=info
Environment=AV_INGEST_PROXY_PORT=$REMOTE_PORT
Environment=AV_INGEST_PROXY_TLS_CERT_PATH=/etc/av-ingest-proxy/local.crt
Environment=AV_INGEST_PROXY_TLS_KEY_PATH=/etc/av-ingest-proxy/local.key
ExecStart=/usr/local/bin/av-ingest-proxy
Restart=always
RestartSec=2
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ReadOnlyPaths=/etc/av-ingest-proxy

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now av-ingest-proxy.service

if command -v caddy >/dev/null 2>&1; then
  install -d /etc/caddy
  touch /etc/caddy/Caddyfile
  cp /etc/caddy/Caddyfile "/etc/caddy/Caddyfile.av-ingest-proxy-backup.$(date +%Y%m%d%H%M%S)"
  python3 - "$PUBLIC_HOSTNAME" "$REMOTE_PORT" <<'PY'
from __future__ import annotations
import pathlib
import sys

host, port = sys.argv[1], sys.argv[2]
path = pathlib.Path("/etc/caddy/Caddyfile")
text = path.read_text()
start = "# av-ingest-proxy BEGIN"
end = "# av-ingest-proxy END"
block = f"""{start}
{host} {{
	reverse_proxy https://127.0.0.1:{port} {{
		flush_interval -1
		transport http {{
			tls_insecure_skip_verify
		}}
	}}
}}
{end}
"""
if start in text and end in text:
    before, rest = text.split(start, 1)
    _, after = rest.split(end, 1)
    text = before.rstrip() + "\n\n" + block + after.lstrip()
else:
    text = text.rstrip() + "\n\n" + block
path.write_text(text)
PY
  caddy validate --config /etc/caddy/Caddyfile
  systemctl enable caddy
  systemctl reload caddy || systemctl restart caddy
fi

systemctl --no-pager --full status av-ingest-proxy.service
ENDSSH
