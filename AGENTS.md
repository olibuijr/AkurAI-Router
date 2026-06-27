# AGENTS.md - AkurAI Router

Docs for this repo live in the **akurai-notes** MCP.

- **Canonical note:** `AkurAI-Router — Docs` (note id 28)
- **Index:** `AkurAI EC2 — Documentation Index` (note id 19)
- **Retrieve:** `search_notes("AkurAI-Router")` or `get_note(28)`
- **Secrets:** none committed; runtime secrets in `/etc/akurai-router/router.env` on EC2 (`akurai-mail`) — see akurai-passvault folder `AkurAI-Router` if entries exist.

## Current implementation notes

- Embeddings are centralized at `POST /v1/embeddings` and `POST /api/v1/embeddings`; Router authenticates the request, rewrites the model to the selected upstream ID, and proxies to `AKURAI_ROUTER_EMBEDDINGS_URL`. Default selected model is `intfloat/multilingual-e5-small`.
- Router app icon/favicon assets are sourced from `/home/olafurbui/Projects/AkurAI-Brand` per `AkurAI-Brand — Docs` (note 6): `icons/favicon/router.svg`, `icons/favicon/router-apple-touch-icon.png`, and `icons/png/router-light-1024.png`.
