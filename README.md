# AkurAI Router

[![Rust](https://img.shields.io/badge/Rust-std--only-b7410e?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![OpenAI compatible](https://img.shields.io/badge/API-OpenAI%20compatible-111827)](#api)
[![Providers](https://img.shields.io/badge/Providers-Codex%20%2B%20Claude%20Code%20%2B%20OpenCode%20Go-2563eb)](#providers)

AkurAI Router is a small OpenAI-compatible router for personal and team tooling.
It exposes a private `/v1` endpoint, protects it with one server-side API key,
and routes model calls through local OAuth credentials from Codex CLI and
Claude Code, plus OpenCode Go subscription API keys.

The project intentionally stays lean: one Rust binary, no Rust crate
dependencies, local config files, and `curl` for outbound HTTPS because the Rust
standard library does not ship a TLS client.

## Features

- OpenAI-compatible `GET /v1/models`
- OpenAI-compatible `POST /v1/chat/completions`
- Codex Responses proxy via Codex CLI OAuth
- Claude Code proxy via Claude Code OAuth and Anthropic Messages
- OpenCode Go provider using OpenAI chat and Anthropic-style messages routes
- Provider-prefixed model IDs such as `codex/gpt-5.4-mini`
- Basic multimodal forwarding for OpenAI `image_url` content on Codex models
- API-key protection for every `/v1/*` route
- Browser admin panel protected by AkurAI IDP SSO
- CLI for provider and model management
- Embedded landing page and static hero asset
- Single std-only Rust binary

## Architecture

```text
OpenAI-compatible client
        |
        | Authorization: Bearer <AKURAI_ROUTER_API_KEY>
        v
  AkurAI Router
        |
        | model owner = codex
        +--> Codex CLI OAuth -> chatgpt.com/backend-api/codex/responses
        |
        | model owner = claude
        +--> Claude Code OAuth -> api.anthropic.com/v1/messages
        |
        | model owner = opencode-go
        +--> OpenCode Go API key -> opencode.ai/zen/go/v1/*
```

The model table records which provider owns each model. The public model list
uses provider prefixes: `codex/gpt-5.4-mini`, `claude/claude-opus-4-8`, and
`opencode-go/glm-5.2`. Bare legacy IDs are still accepted for existing clients.

## API

| Route | Method | Auth | Description |
| --- | --- | --- | --- |
| `/` | `GET` | public | Landing page |
| `/health` | `GET` | public | Service health |
| `/login` | `GET` | browser | Start AkurAI IDP login |
| `/admin` | `GET` | SSO | Provider and model admin |
| `/v1/models` | `GET` | bearer key | OpenAI-compatible model list |
| `/v1/chat/completions` | `POST` | bearer key | OpenAI-compatible chat |
| `/v1/responses` | `POST` | bearer key | Codex Responses passthrough |

Example:

```bash
curl "$AKURAI_ROUTER_BASE/v1/chat/completions" \
  -H "Authorization: Bearer $AKURAI_ROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "opencode-go/glm-5.2",
    "messages": [
      { "role": "user", "content": "Reply with only OK." }
    ]
  }'
```

## Providers

### Codex

Codex routing follows the minimal 9Router-style Codex path:

- OAuth source: `~/.codex/auth.json`
- Upstream: `https://chatgpt.com/backend-api/codex/responses`
- Headers include `originator: codex_cli_rs`, Codex CLI user-agent,
  `session_id`, and `chatgpt-account-id` when available
- Chat Completions requests are translated to Responses requests
- Responses requests are normalized for Codex defaults

### Claude Code

Claude routing uses the Claude Code OAuth credential file:

- OAuth source: `~/.claude/.credentials.json`
- Upstream: `https://api.anthropic.com/v1/messages`
- OpenAI chat messages are translated to Anthropic Messages
- Anthropic responses are normalized back into OpenAI chat-completions JSON
- Claude Code identity and OAuth beta headers are sent with each request

Known usable Claude Code defaults currently include:

- `claude-opus-4-8`
- `claude-opus-4-7`
- `claude-opus-4-6`
- `claude-sonnet-4-6`
- `claude-opus-4-5-20251101`
- `claude-haiku-4-5-20251001`
- `claude-sonnet-4-5-20250929`
- `claude-opus-4-1-20250805`

`claude-fable-5` may appear in the Anthropic OAuth catalog, but it returned
`404` through this Claude Code Messages path during verification, so the router
filters it out of catalog syncs.

### OpenCode Go

OpenCode Go routing follows the 9Router OpenCode Go provider shape:

- API key source: `~/.local/share/opencode/auth.json`
- Chat upstream: `https://opencode.ai/zen/go/v1/chat/completions`
- Messages upstream: `https://opencode.ai/zen/go/v1/messages`
- GLM, Kimi, DeepSeek, and MiMo models use OpenAI-compatible chat with bearer auth
- MiniMax and Qwen models use Anthropic-style messages with `x-api-key`

Default OpenCode Go models include:

- `glm-5.2`
- `glm-5.1`
- `kimi-k2.7-code`
- `kimi-k2.6`
- `deepseek-v4-pro`
- `deepseek-v4-flash`
- `mimo-v2.5`
- `mimo-v2.5-pro`
- `minimax-m3`
- `minimax-m2.7`
- `minimax-m2.5`
- `qwen3.7-max`
- `qwen3.7-plus`
- `qwen3.6-plus`

## Quickstart

Build the binary:

```bash
cargo check
cargo build --release
```

Create local config:

```bash
./target/release/akurai-router init
```

Generate an API key:

```bash
./target/release/akurai-router key generate
```

Configure credentials in `~/.akurai-router/router.conf` or environment
variables:

```bash
AKURAI_ROUTER_LISTEN=127.0.0.1:4219
AKURAI_ROUTER_PUBLIC_URL=http://127.0.0.1:4219
AKURAI_ROUTER_API_KEY=akr_...
AKURAI_ROUTER_COOKIE_SECRET=...
AKURAI_ROUTER_CODEX_AUTH_PATH=/home/you/.codex/auth.json
AKURAI_ROUTER_CLAUDE_AUTH_PATH=/home/you/.claude/.credentials.json
AKURAI_ROUTER_OPENCODE_GO_AUTH_PATH=/home/you/.local/share/opencode/auth.json
AKURAI_ROUTER_DEFAULT_MODEL=gpt-5.4-mini
AKURAI_ROUTER_IDP_ISSUER=https://auth.example.com
AKURAI_ROUTER_IDP_CLIENT_ID=...
AKURAI_ROUTER_IDP_CLIENT_SECRET=...
AKURAI_ROUTER_ADMIN_EMAIL=you@example.com
AKURAI_ROUTER_HOME=/home/you/.akurai-router
```

Run:

```bash
AKURAI_ROUTER_API_KEY=akr_... \
AKURAI_ROUTER_COOKIE_SECRET=change-me-to-a-long-random-string \
cargo run -- serve
```

## CLI

```bash
akurai-router serve
akurai-router init
akurai-router key generate

akurai-router providers list
akurai-router providers add codex --auth-path ~/.codex/auth.json
akurai-router providers add claude --auth-path ~/.claude/.credentials.json
akurai-router providers add opencode-go --auth-path ~/.local/share/opencode/auth.json
akurai-router providers enable codex
akurai-router providers disable claude

akurai-router models list
akurai-router models add gpt-5.4-mini "GPT 5.4 Mini" gpt-5.4-mini codex
akurai-router models add claude-opus-4-8 "Claude Opus 4.8" claude-opus-4-8 claude
akurai-router models add glm-5.2 "GLM 5.2" glm-5.2 opencode-go
akurai-router models remove claude-opus-4-8

akurai-router idp client-json
akurai-router idp env
```

Model rows are stored as:

```text
id|name|upstream_id|provider_id|enabled
```

Provider rows are stored as:

```text
id|name|enabled|auth_path
```

`/v1/models` returns provider-prefixed IDs. Store model rows with the bare
provider-local ID unless you intentionally want a custom public alias.

## Admin Panel

The admin panel is available at `/admin` and requires AkurAI IDP SSO. The router
checks the `userinfo.email` value against `AKURAI_ROUTER_ADMIN_EMAIL` before
showing provider configuration.

Register an IDP client with the callback URL printed by:

```bash
akurai-router idp client-json
```

For production, use an HTTPS public URL and set:

```bash
AKURAI_ROUTER_PUBLIC_URL=https://router.example.com
```

## Deployment

The included `deploy.sh` builds a static musl Linux artifact and installs a
systemd service on the configured host:

```bash
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --target x86_64-unknown-linux-musl

AKURAI_ROUTER_DEPLOY_HOST=my-router-host ./deploy.sh
```

The service binds to `127.0.0.1:4219` by default. Put nginx, Caddy, or another
TLS reverse proxy in front of it.

Recommended service layout:

```text
TLS reverse proxy -> 127.0.0.1:4219 -> akurai-router -> provider OAuth upstreams
```

## Security

- Do not expose `/v1/*` without `AKURAI_ROUTER_API_KEY`.
- Do not commit `router.conf`, `/etc/akurai-router/router.env`, OAuth files, or
  copied credentials.
- Keep `~/.codex/auth.json`, `~/.claude/.credentials.json`, and
  `~/.local/share/opencode/auth.json` readable only by the service account.
- Use HTTPS for public deployments.
- Rotate the router API key if it is pasted into logs, chat, screenshots, or
  client-side code.
- The router uses local OAuth material from CLI tools. Treat the host as a
  trusted personal or team runtime.

## Development

```bash
cargo fmt --check
cargo check
cargo test
```

Project constraints:

- No Rust crate dependencies in `Cargo.toml`
- Rust `std` only
- Outbound HTTPS through host `curl`
- Small, explicit config files instead of a database

## License

MIT. See [LICENSE](LICENSE).
