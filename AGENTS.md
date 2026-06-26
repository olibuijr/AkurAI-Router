# AGENTS.md - AkurAI Router

## Scope

AkurAI Router is a standalone, std-only Rust OpenAI-compatible router. It exposes an API-key-protected `/v1` surface, routes to Codex CLI and Claude Code OAuth upstreams, and serves an AkurAI IDP-protected admin panel.

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
- AkurAI IDP issuer: `https://auth.olibuijr.com`
- Admin allowlist: `olibuijr@olibuijr.com`

## Providers and Models

- Default providers are `codex` and `claude`; model ownership is recorded in `models.conf`.
- Current verified Claude Code chat models include `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-opus-4-5-20251101`, `claude-haiku-4-5-20251001`, `claude-sonnet-4-5-20250929`, and `claude-opus-4-1-20250805`.
- `claude-fable-5` appears in the Anthropic OAuth catalog but returned 404 through the Claude Code Messages path during 2026-06-26 verification; keep it filtered unless a live smoke proves it usable.

## Invariants

- Keep `Cargo.toml` free of dependencies; the binary must remain Rust `std` only.
- TLS terminates at nginx. Outbound HTTPS uses host `curl` because std has no TLS.
- Do not commit API keys, IDP client secrets, Codex tokens, Claude tokens, or host env files.
- Admin UI login must go through AkurAI IDP and verify `userinfo.email` against the allowlist before serving `/admin`.
- `/v1/*` endpoints must require `Authorization: Bearer <AKURAI_ROUTER_API_KEY>`.
- Keep `README.md` GitHub/open-source oriented. Put private host state, credential paths, and deployment observations in this `AGENTS.md` file or root workspace AGENTS state, not in public-facing docs.
