#!/usr/bin/env bash
set -euo pipefail

TARGET="x86_64-unknown-linux-musl"
HOST="${AKURAI_ROUTER_DEPLOY_HOST:-akurai-mail}"
REMOTE_BIN="/usr/local/bin/akurai-router"
SERVICE="akurai-router.service"
PORT="${AKURAI_ROUTER_PORT:-4219}"

CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}" \
  cargo build --release --target "$TARGET"

ssh "$HOST" 'sudo mkdir -p /etc/akurai-router /var/lib/akurai-router /home/ubuntu/.claude /home/ubuntu/.local/share/opencode && sudo chown ubuntu:ubuntu /home/ubuntu/.claude /home/ubuntu/.local /home/ubuntu/.local/share /home/ubuntu/.local/share/opencode && sudo chmod 0700 /home/ubuntu/.claude /home/ubuntu/.local/share/opencode'
scp "target/$TARGET/release/akurai-router" "$HOST:/tmp/akurai-router"
ssh "$HOST" "sudo install -m 0755 /tmp/akurai-router $REMOTE_BIN && rm -f /tmp/akurai-router"

ssh "$HOST" "if [ ! -f /etc/akurai-router/router.env ]; then
  API_KEY=\"akr_\$(openssl rand -hex 32)\"
  COOKIE_SECRET=\"\$(openssl rand -hex 48)\"
  sudo tee /etc/akurai-router/router.env >/dev/null <<EOF
AKURAI_ROUTER_LISTEN=127.0.0.1:$PORT
AKURAI_ROUTER_PUBLIC_URL=https://akurai-router.olibuijr.com
AKURAI_ROUTER_API_KEY=\$API_KEY
AKURAI_ROUTER_COOKIE_SECRET=\$COOKIE_SECRET
AKURAI_ROUTER_CODEX_AUTH_PATH=/home/ubuntu/.codex/auth.json
AKURAI_ROUTER_CLAUDE_AUTH_PATH=/home/ubuntu/.claude/.credentials.json
AKURAI_ROUTER_OPENCODE_GO_AUTH_PATH=/home/ubuntu/.local/share/opencode/auth.json
AKURAI_ROUTER_DEFAULT_MODEL=gpt-5.4-mini
AKURAI_ROUTER_IDP_ISSUER=https://auth.olibuijr.com
AKURAI_ROUTER_IDP_CLIENT_ID=
AKURAI_ROUTER_IDP_CLIENT_SECRET=
AKURAI_ROUTER_ADMIN_EMAIL=olibuijr@olibuijr.com
AKURAI_ROUTER_HOME=/var/lib/akurai-router
EOF
  sudo chmod 0600 /etc/akurai-router/router.env
fi"

ssh "$HOST" "if ! sudo grep -q '^AKURAI_ROUTER_OPENCODE_GO_AUTH_PATH=' /etc/akurai-router/router.env; then
  echo 'AKURAI_ROUTER_OPENCODE_GO_AUTH_PATH=/home/ubuntu/.local/share/opencode/auth.json' | sudo tee -a /etc/akurai-router/router.env >/dev/null
fi"

ssh "$HOST" "sudo tee /etc/systemd/system/$SERVICE >/dev/null <<'EOF'
[Unit]
Description=AkurAI Router
After=network-online.target
Wants=network-online.target

[Service]
User=ubuntu
Group=ubuntu
EnvironmentFile=/etc/akurai-router/router.env
WorkingDirectory=/var/lib/akurai-router
ExecStart=/usr/local/bin/akurai-router serve
Restart=always
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=read-only
ReadWritePaths=/var/lib/akurai-router /home/ubuntu/.codex /home/ubuntu/.claude /home/ubuntu/.local/share/opencode

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable $SERVICE
sudo systemctl restart $SERVICE
sleep 1
curl -fsS http://127.0.0.1:$PORT/health"
