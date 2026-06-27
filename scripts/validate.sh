#!/usr/bin/env bash
# scripts/validate.sh — Post-deploy validation for AkurAI-Router
set -euo pipefail

DOMAIN="akurai-router.olibuijr.com"
PORT=4219
RED='\033[0;31m'; GRN='\033[0;32m'; NC='\033[0m'
pass=0; fail=0
pass_() { printf "  ${GRN}PASS${NC} %s\n" "$*"; ((pass++)); }
fail_() { printf "  ${RED}FAIL${NC} %s\n" "$*"; ((fail++)); }

echo "=== Post-deploy validation: AkurAI-Router ==="

# 1. Systemd
systemctl is-active --quiet akurai-router.service 2>/dev/null && pass_ "systemd active" || fail_ "systemd not active"

# 2. Loopback health
if curl -fsS --max-time 5 "http://127.0.0.1:${PORT}/health" > /dev/null 2>&1; then
  pass_ "loopback /health"
else
  fail_ "loopback /health unreachable"
fi

# 3. Public HTTPS health
if curl -fsS --max-time 10 "https://${DOMAIN}/health" > /dev/null 2>&1; then
  pass_ "public /health"
else
  fail_ "public /health unreachable"
fi

# 4. OIDC login redirect (Router has dashboard with OIDC SSO)
LOGIN_STATUS=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "https://${DOMAIN}/login" 2>/dev/null)
[ "$LOGIN_STATUS" = "200" ] || [ "$LOGIN_STATUS" = "302" ] && pass_ "/login reachable (${LOGIN_STATUS})" || fail_ "/login → ${LOGIN_STATUS}"

# 5. Embedding endpoint (internal only)
EMBED_STATUS=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 \
  -X POST "http://127.0.0.1:${PORT}/v1/embeddings" \
  -H "Content-Type: application/json" \
  -d '{"input":"test","model":"intfloat/multilingual-e5-small"}' 2>/dev/null)
[ "$EMBED_STATUS" = "200" ] || [ "$EMBED_STATUS" = "401" ] || [ "$EMBED_STATUS" = "400" ] && pass_ "/v1/embeddings reachable (${EMBED_STATUS})" || fail_ "/v1/embeddings → ${EMBED_STATUS}"

# 6. Env file
[ -f "/etc/akurai-router/router.env" ] && pass_ "router.env present" || fail_ "router.env missing"

echo "━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GRN}Pass: $pass${NC}  ${RED}Fail: $fail${NC}"
[ "$fail" -eq 0 ] || exit 1
