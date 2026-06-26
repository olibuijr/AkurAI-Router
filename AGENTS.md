# AGENTS.md - AkurAI Router

## Scope

AkurAI Router is a standalone, std-only Rust OpenAI-compatible router. It exposes an API-key-protected `/v1` surface, routes to Codex CLI OAuth, Claude Code OAuth, and OpenCode Go subscription upstreams, and serves an AkurAI IDP-protected admin panel.

## Commands

- Check: `cargo check`
- Format: `cargo fmt --check`
- Build VM artifact: `CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target x86_64-unknown-linux-musl`
- Run locally: `AKURAI_ROUTER_API_KEY=... AKURAI_ROUTER_COOKIE_SECRET=... AKURAI_ROUTER_IDP_CLIENT_ID=... AKURAI_ROUTER_IDP_CLIENT_SECRET=... cargo run -- serve`
- Deploy: `./deploy.sh`
- Test: `cargo test`

## Runtime

- Production URL: `https://akurai-router.olibuijr.com`
- VM: `akurai-mail` / `mail.olibuijr.com`
- Service: `akurai-router.service`
- Listen address: `127.0.0.1:4219`
- Env file: `/etc/akurai-router/router.env`
- Runtime data: `/var/lib/akurai-router`
- Codex OAuth source: `/home/ubuntu/.codex/auth.json`
- Claude Code OAuth source: `/home/ubuntu/.claude/.credentials.json` on the router host, mirrored from `/home/olafurbui/.claude/.credentials.json` on `midget` when needed
- OpenCode Go auth source: `/home/ubuntu/.local/share/opencode/auth.json` on the router host, mirrored from `/home/olafurbui/.local/share/opencode/auth.json` when needed. Do not print key material.
- Admin/accounting state lives in `/var/lib/akurai-router/users.conf`, `client_keys.conf`, `billing.conf`, and `usage.tsv`. `client_keys.conf` contains generated router API keys; never print or commit it.
- AkurAI IDP issuer: `https://auth.olibuijr.com`
- Admin allowlist: `olibuijr@olibuijr.com`

## Providers and Models

- Default providers are `codex`, `claude`, and `opencode-go`; model ownership is recorded in `models.conf`.
- `/v1/models` publishes provider-prefixed model IDs (`codex/...`, `claude/...`, `opencode-go/...`). Routing still accepts bare legacy IDs plus aliases `cx/...`, `cc/...`, `opencode/...`, and `ocg/...`.
- The admin dashboard manages IDP users by email, generated per-user router API keys, monthly shared-cost allocation percentages, and a per-request usage ledger. The legacy global `AKURAI_ROUTER_API_KEY` remains valid and is attributed to the admin allowlist email.
- Current verified Claude Code chat models include `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-opus-4-5-20251101`, `claude-haiku-4-5-20251001`, `claude-sonnet-4-5-20250929`, and `claude-opus-4-1-20250805`.
- `claude-fable-5` appears in the Anthropic OAuth catalog but returned 404 through the Claude Code Messages path during 2026-06-26 verification; keep it filtered unless a live smoke proves it usable.
- OpenCode Go defaults mirror the 9router OpenCode Go catalog from 2026-06-26: chat-format `glm-5.2`, `glm-5.1`, `kimi-k2.7-code`, `kimi-k2.6`, `deepseek-v4-pro`, `deepseek-v4-flash`, `mimo-v2.5`, `mimo-v2.5-pro`; Anthropic messages-format `minimax-m3`, `minimax-m2.7`, `minimax-m2.5`, `qwen3.7-max`, `qwen3.7-plus`, `qwen3.6-plus`.

## Invariants

- Keep `Cargo.toml` free of dependencies; the binary must remain Rust `std` only.
- TLS terminates at nginx. Outbound HTTPS uses host `curl` because std has no TLS.
- Do not commit API keys, IDP client secrets, Codex tokens, Claude tokens, or host env files.
- Do not commit OpenCode API keys or copy them into logs; inspect only redacted structure when debugging auth files.
- Do not print generated user router keys except on the intentional one-time admin creation page.
- Admin UI login must go through AkurAI IDP and verify `userinfo.email` against the allowlist before serving `/admin`.
- `/v1/*` endpoints must require `Authorization: Bearer <AKURAI_ROUTER_API_KEY>`.
- Keep `README.md` GitHub/open-source oriented. Put private host state, credential paths, and deployment observations in this `AGENTS.md` file or root workspace AGENTS state, not in public-facing docs.
