# AkurAI Router

AkurAI Router is a minimal, std-only Rust OpenAI-compatible endpoint for routing requests to the Codex CLI and Claude Code OAuth backends.

It serves:

- `GET /` open-source landing page
- `GET /admin` AkurAI IDP-protected provider/model admin UI
- `GET /v1/models` OpenAI-compatible model list
- `POST /v1/responses` Codex Responses API proxy
- `POST /v1/chat/completions` best-effort Chat Completions proxy for Codex and Claude models

Both `/v1` chat/responses endpoints accept **multimodal input**: OpenAI
`image_url` content parts (object `{url, detail}` or bare string; `data:` URIs and
`https://` URLs) are forwarded upstream as Responses-API `input_image`, so
vision / computer-use requests reach the model with their images intact.

The API surface uses a private bearer key for tooling. The admin UI uses AkurAI IDP SSO and only allows `olibuijr@olibuijr.com` by default.

## Design

The binary uses only Rust `std`; `Cargo.toml` has no dependencies. TLS terminates at nginx. Outbound HTTPS to `chatgpt.com` and the AkurAI IDP token/userinfo endpoints is performed through the host `curl` binary, because Rust `std` does not provide TLS.

Codex upstream behavior follows the minimal 9Router Codex path:

- upstream URL: `https://chatgpt.com/backend-api/codex/responses`
- bearer token from `~/.codex/auth.json`
- `originator: codex_cli_rs`
- `User-Agent: codex_cli_rs/0.136.0`
- `session_id` and `chatgpt-account-id` headers when available
- `stream=true`, `store=false`, default Codex instructions, and Responses-compatible request cleanup

Claude Code upstream behavior uses the Claude Code OAuth file on disk:

- bearer token from `~/.claude/.credentials.json`
- Claude Code spoof headers and the Anthropic Messages API
- OpenAI chat input is translated to Anthropic text messages and normalized back to OpenAI chat-completions JSON

## Build

```bash
cargo check
cargo build --release
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target x86_64-unknown-linux-musl
```

## Configure

```bash
akurai-router init
```

Primary settings:

```bash
AKURAI_ROUTER_LISTEN=127.0.0.1:4219
AKURAI_ROUTER_PUBLIC_URL=https://akurai-router.olibuijr.com
AKURAI_ROUTER_API_KEY=akr_...
AKURAI_ROUTER_COOKIE_SECRET=...
AKURAI_ROUTER_CODEX_AUTH_PATH=/home/ubuntu/.codex/auth.json
AKURAI_ROUTER_CLAUDE_AUTH_PATH=/home/olafurbui/.claude/.credentials.json
AKURAI_ROUTER_IDP_ISSUER=https://auth.olibuijr.com
AKURAI_ROUTER_IDP_CLIENT_ID=...
AKURAI_ROUTER_IDP_CLIENT_SECRET=...
AKURAI_ROUTER_ADMIN_EMAIL=olibuijr@olibuijr.com
```

## AkurAI IDP Client

Register an OIDC client in AkurAI IDP with this redirect URI:

```text
https://akurai-router.olibuijr.com/auth/callback
```

The helper prints the payload shape:

```bash
akurai-router idp client-json
```

Set `first_party: true` so the IDP auto-approves after `olibuijr@olibuijr.com` signs in.

## CLI

```bash
akurai-router serve
akurai-router key generate
akurai-router providers list
akurai-router providers add codex --auth-path ~/.codex/auth.json
akurai-router providers add claude --auth-path ~/.claude/.credentials.json
akurai-router providers enable [ID]
akurai-router providers disable [ID]
akurai-router models list
akurai-router models add gpt-5.4-mini "GPT 5.4 Mini" gpt-5.4-mini codex
akurai-router models add claude-sonnet-4-6 "Claude Sonnet 4.6" claude-sonnet-4-6 claude
akurai-router models remove gpt-5.4-mini
akurai-router idp client-json
```

## Client Usage

Configure OpenAI-compatible tools with:

```text
Base URL: https://akurai-router.olibuijr.com/v1
API key: the value of AKURAI_ROUTER_API_KEY
Model: gpt-5.4-mini or claude-sonnet-4-6
```

Codex CLI should use `wire_api = "responses"` for this endpoint.

## Deployment

The intended production host is the AkurAI EC2 VM:

```text
nginx TLS -> 127.0.0.1:4219 -> akurai-router -> curl -> Codex / Claude Code OAuth upstreams
```

Run:

```bash
./deploy.sh
```

Then create or update the nginx vhost for `akurai-router.olibuijr.com`.
