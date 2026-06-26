# AGENTS.md - AkurAI Router

## Scope

AkurAI Router is a standalone, std-only Rust OpenAI-compatible router. It exposes an API-key-protected `/v1` surface and an AkurAI IDP-protected admin panel.

## Commands

- Check: `cargo check`
- Format: `cargo fmt --check`
- Build VM artifact: `CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target x86_64-unknown-linux-musl`
- Run locally: `AKURAI_ROUTER_API_KEY=... AKURAI_ROUTER_COOKIE_SECRET=... AKURAI_ROUTER_IDP_CLIENT_ID=... AKURAI_ROUTER_IDP_CLIENT_SECRET=... cargo run -- serve`
- Deploy: `./deploy.sh`

## Runtime

- Production URL: `https://akurai-router.olibuijr.com`
- VM: `akurai-mail` / `mail.olibuijr.com`
- Service: `akurai-router.service`
- Listen address: `127.0.0.1:4219`
- Env file: `/etc/akurai-router/router.env`
- Runtime data: `/var/lib/akurai-router`
- Codex OAuth source: `/home/ubuntu/.codex/auth.json`
- Claude Code OAuth source: `/home/olafurbui/.claude/.credentials.json` on the source machine; mirror the file onto the router host before enabling the provider
- AkurAI IDP issuer: `https://auth.olibuijr.com`
- Admin allowlist: `olibuijr@olibuijr.com`

## Invariants

- Keep `Cargo.toml` free of dependencies; the binary must remain Rust `std` only.
- TLS terminates at nginx. Outbound HTTPS uses host `curl` because std has no TLS.
- Do not commit API keys, IDP client secrets, Codex tokens, or VM env files.
- Default providers are `codex` and `claude`; model ownership is recorded in `models.conf`.
- Admin UI login must go through AkurAI IDP and verify `userinfo.email` against the allowlist before serving `/admin`.
- `/v1/*` endpoints must require `Authorization: Bearer <AKURAI_ROUTER_API_KEY>`.
