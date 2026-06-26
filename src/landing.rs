use std::net::TcpStream;
use std::path::PathBuf;

use crate::accounts::{self, BillingConfig, RouterUser, UsageSummary};
use crate::auth;
use crate::config::{
    Config, Model, Provider, canonical_provider_id, default_provider_auth_path,
    infer_model_provider_id, load_models, load_providers, model_id_for_provider,
    provider_display_name, public_model_id, save_models, save_providers,
};
use crate::http::{self, Request};
use crate::upstream;
use crate::util::{html_escape, parse_query, query_get};

const HERO: &[u8] = include_bytes!("../assets/hero.png");

pub fn hero(stream: &mut TcpStream) {
    let _ = http::send_response(
        stream,
        200,
        "OK",
        &[
            ("Content-Type", "image/png".to_string()),
            (
                "Cache-Control",
                "public, max-age=31536000, immutable".to_string(),
            ),
        ],
        HERO,
    );
}

pub fn landing(req: &Request, stream: &mut TcpStream, cfg: &Config) {
    let logged_in = auth::admin_session(req, cfg).is_some();
    let admin_link = if logged_in {
        r#"<a class="button secondary" href="/admin">Admin</a>"#
    } else {
        r#"<a class="button secondary" href="/login">Log in</a>"#
    };
    let html = format!(
        r#"<!doctype html>
<html lang="en" data-theme="">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="description" content="AkurAI Router is a std-only Rust OpenAI-compatible endpoint for Codex, Claude Code, and OpenCode Go routing.">
  <meta property="og:title" content="AkurAI Router">
  <meta property="og:description" content="A minimal Rust OpenAI-compatible router with provider-prefixed model routing.">
  <meta property="og:image" content="{base}/assets/hero.png">
  <title>AkurAI Router</title>
  <script>{flash_js}</script>
  <style>{themes}{css}</style>
</head>
<body>
  <main class="hero">
    <img src="/assets/hero.png" alt="" class="hero-img">
    <div class="shade"></div>
    <nav>
      <a class="brand" href="/">AkurAI Router</a>
      <div>
        <a href="/v1/models">Models</a>
        <a href="https://github.com/olibuijr/AkurAI-Router">GitHub</a>
        {admin_link}
        <span data-theme-picker></span>
      </div>
    </nav>
    <section class="copy">
      <p class="eyebrow">OpenAI endpoint. Provider-prefixed routing.</p>
      <h1>AkurAI Router</h1>
      <p class="lead">A small Rust service that exposes `/v1/responses`, `/v1/chat/completions`, and `/v1/models`, authenticates tools with a private API key, and routes requests to Codex, Claude Code, or OpenCode Go.</p>
      <div class="actions">
        <a class="button" href="/login">Log in with AkurAI IDP</a>
        <a class="button ghost" href="https://github.com/olibuijr/AkurAI-Router">Source</a>
      </div>
    </section>
  </main>
  <section class="band">
    <div class="grid">
      <div><h2>One binary</h2><p>Rust standard library server, embedded landing asset, local config files, and no Rust crate dependencies.</p></div>
      <div><h2>Protected API</h2><p>Tooling calls use `Authorization: Bearer ...`; the admin UI uses AkurAI IDP SSO and an email allowlist.</p></div>
      <div><h2>Provider native</h2><p>Model IDs use `codex/`, `claude/`, and `opencode-go/` prefixes while upstream auth stays server-side.</p></div>
    </div>
  </section>
  <script>{picker_js}</script>
</body>
</html>"#,
        base = cfg.public_base_url,
        flash_js = flash_guard_js(),
        themes = themes_css(),
        css = style(),
        picker_js = theme_picker_js(),
        admin_link = admin_link,
    );
    let _ = http::send_text(stream, 200, "text/html; charset=utf-8", &html);
}

pub fn admin(req: &Request, stream: &mut TcpStream, cfg: &Config) {
    let Some(session) = auth::admin_session(req, cfg) else {
        let _ = http::redirect(stream, "/login", &[]);
        return;
    };
    let _ = accounts::ensure_account_files(cfg);
    let providers = load_providers(cfg);
    let models = load_models(cfg);
    let users = accounts::load_users(cfg);
    let keys = accounts::load_client_keys(cfg);
    let billing = accounts::load_billing_config(cfg);
    let usage = accounts::usage_summaries(cfg);
    let auth_ok = providers.iter().any(|p| p.enabled && p.auth_path.exists());
    let total_requests: u64 = usage.iter().map(|u| u.requests).sum();
    let total_tokens: u64 = usage.iter().map(|u| u.total_tokens).sum();
    let total_direct_cost: f64 = usage.iter().map(|u| u.cost_usd).sum();
    let active_keys = keys.iter().filter(|k| k.enabled).count();
    let provider_rows = providers
        .iter()
        .map(|p| {
            let status_class = if p.enabled && p.auth_path.exists() {
                "pill ok"
            } else {
                "pill warn"
            };
            let status = if p.enabled && p.auth_path.exists() {
                "auth found"
            } else {
                "auth missing"
            };
            format!(
                r#"<form method="post" action="/admin/provider/save" class="provider-form">
  <input type="hidden" name="id" value="{}">
  <div class="provider-main">
    <strong>{}</strong>
    <span class="muted">{}</span>
  </div>
  <label>Auth path<input name="auth_path" value="{}"></label>
  <label class="check"><input type="checkbox" name="enabled" {}> Enabled</label>
  <span class="{}">{}</span>
  <button>Save</button>
</form>"#,
                html_escape(&p.id),
                html_escape(&p.name),
                html_escape(&p.id),
                html_escape(&p.auth_path.display().to_string()),
                if p.enabled { "checked" } else { "" },
                status_class,
                status
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let user_rows = users
        .iter()
        .map(|user| {
            let summary = usage_summary_for(&usage, &user.email);
            let allocated = summary.cost_usd
                + billing.monthly_shared_cost_usd * user.cost_share_pct.max(0.0) / 100.0;
            format!(
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.2}%</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>"#,
                html_escape(&user.email),
                html_escape(&user.name),
                if user.enabled { "enabled" } else { "disabled" },
                user.cost_share_pct,
                summary.requests,
                format_u64(summary.total_tokens),
                money(summary.cost_usd),
                money(allocated),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let key_rows = keys
        .iter()
        .map(|key| {
            format!(
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><form method="post" action="/admin/keys/revoke"><input type="hidden" name="id" value="{}"><input type="hidden" name="enabled" value="{}"><button>{}</button></form></td></tr>"#,
                html_escape(&key.id),
                html_escape(&key.email),
                html_escape(&key.name),
                html_escape(&accounts::key_hint(&key.key)),
                if key.enabled { "enabled" } else { "disabled" },
                if key.last_used_at == 0 { "never".to_string() } else { key.last_used_at.to_string() },
                html_escape(&key.id),
                if key.enabled { "false" } else { "true" },
                if key.enabled { "Revoke" } else { "Enable" },
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let user_options = users
        .iter()
        .filter(|u| u.enabled)
        .map(|u| {
            format!(
                r#"<option value="{}">{}</option>"#,
                html_escape(&u.email),
                html_escape(&format!("{} ({})", u.name, u.email))
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let model_rows = models
        .iter()
        .map(|m| {
            format!(
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><form method="post" action="/admin/models/remove"><input type="hidden" name="id" value="{}"><button class="icon" title="Remove">×</button></form></td></tr>"#,
                html_escape(&public_model_id(m)),
                html_escape(&m.name),
                html_escape(&m.upstream_id),
                html_escape(&m.provider_id),
                if m.enabled { "enabled" } else { "disabled" },
                html_escape(&m.id),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let html = format!(
        r#"<!doctype html>
<html lang="en" data-theme="">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>AkurAI Router Admin</title><script>{flash_js}</script><style>{themes}{css}</style></head>
<body class="admin-body">
<header class="topbar"><a class="brand" href="/">AkurAI Router</a><nav><span>{email}</span><a href="/logout">Log out</a><span data-theme-picker></span></nav></header>
<main class="admin">
  <section class="panel wide">
    <div><p class="eyebrow">Endpoint</p><h1>{base}/v1</h1></div>
    <div class="status-row"><span class="{status_class}">{status}</span><span>Session expires at {expires}</span></div>
    <div class="metric-grid">
      <div class="metric"><span>Requests</span><strong>{total_requests}</strong></div>
      <div class="metric"><span>Tokens</span><strong>{total_tokens}</strong></div>
      <div class="metric"><span>Direct cost</span><strong>{total_direct_cost}</strong></div>
      <div class="metric"><span>Active keys</span><strong>{active_keys}</strong></div>
    </div>
  </section>

  <section class="panel">
    <h2>Providers</h2>
    <div class="provider-list">{provider_rows}</div>
    <p class="muted">{provider_hint}</p>
  </section>

  <section class="panel">
    <h2>Login Setup</h2>
    <ol class="steps">
      <li>Refresh Codex OAuth with `codex login` for the configured service account when renewal is required.</li>
      <li>Keep configured Codex, Claude Code, and OpenCode Go auth files readable only by the service account.</li>
      <li>Configure clients with base URL `{base}/v1`, wire API `responses`, and the router API key from `/etc/akurai-router/router.env`.</li>
    </ol>
  </section>

  <section class="panel wide">
    <div class="row-head"><h2>IDP Users and Cost Allocation</h2><form method="post" action="/admin/billing/save" class="mini-form"><label>Monthly shared cost USD<input name="monthly_shared_cost_usd" type="number" min="0" step="0.0001" value="{monthly_shared_cost}"></label><button>Save</button></form></div>
    <table><thead><tr><th>Email</th><th>Name</th><th>Status</th><th>Share</th><th>Requests</th><th>Tokens</th><th>Direct cost</th><th>Allocated total</th></tr></thead><tbody>{user_rows}</tbody></table>
    <form method="post" action="/admin/users/save" class="inline-form user-add">
      <input name="email" placeholder="idp user email">
      <input name="name" placeholder="display name">
      <input name="cost_share_pct" type="number" min="0" max="100" step="0.01" placeholder="cost share %">
      <label class="check"><input type="checkbox" name="enabled" checked> Enabled</label>
      <button>Add/update user</button>
    </form>
  </section>

  <section class="panel wide">
    <h2>Assigned Router Keys</h2>
    <table><thead><tr><th>ID</th><th>Owner</th><th>Name</th><th>Key</th><th>Status</th><th>Last used</th><th></th></tr></thead><tbody>{key_rows}</tbody></table>
    <form method="post" action="/admin/keys/create" class="inline-form key-add">
      <select name="email">{user_options}</select>
      <input name="name" placeholder="key label">
      <button>Create key</button>
    </form>
  </section>

  <section class="panel wide">
    <div class="row-head"><h2>Models</h2><form method="post" action="/admin/models/sync"><button>Sync provider catalogs</button></form></div>
    <table><thead><tr><th>ID</th><th>Name</th><th>Upstream</th><th>Provider</th><th>Status</th><th></th></tr></thead><tbody>{model_rows}</tbody></table>
    <form method="post" action="/admin/models/add" class="inline-form">
      <input name="id" placeholder="model id">
      <input name="name" placeholder="display name">
      <input name="upstream_id" placeholder="upstream id">
      <input name="provider_id" placeholder="provider id">
      <button>Add model</button>
    </form>
  </section>
</main>
<script>{picker_js}</script>
</body>
</html>"#,
        flash_js = flash_guard_js(),
        themes = themes_css(),
        css = style(),
        picker_js = theme_picker_js(),
        email = html_escape(&session.email),
        expires = session.expires_at,
        base = html_escape(&cfg.public_base_url),
        status_class = if auth_ok { "pill ok" } else { "pill warn" },
        status = if auth_ok {
            "Provider auth found"
        } else {
            "Provider auth missing"
        },
        total_requests = format_u64(total_requests),
        total_tokens = format_u64(total_tokens),
        total_direct_cost = money(total_direct_cost),
        active_keys = active_keys,
        monthly_shared_cost = format!("{:.4}", billing.monthly_shared_cost_usd),
        provider_hint = if auth_ok {
            "Keep each provider auth file readable only by the service account. Codex uses ~/.codex/auth.json; Claude Code uses ~/.claude/.credentials.json; OpenCode Go uses ~/.local/share/opencode/auth.json."
        } else {
            "Point each provider at the correct local auth file. Codex uses ~/.codex/auth.json; Claude Code uses ~/.claude/.credentials.json; OpenCode Go uses ~/.local/share/opencode/auth.json."
        },
        provider_rows = provider_rows,
        user_rows = user_rows,
        key_rows = key_rows,
        user_options = user_options,
        model_rows = model_rows,
    );
    let _ = http::send_text(stream, 200, "text/html; charset=utf-8", &html);
}

pub fn admin_post(req: &Request, stream: &mut TcpStream, cfg: &Config) {
    let Some(session) = auth::admin_session(req, cfg) else {
        let _ = http::send_text(stream, 401, "text/plain", "login required");
        return;
    };
    if !session.email.eq_ignore_ascii_case(&cfg.admin_allowed_email) {
        let _ = http::send_text(stream, 403, "text/plain", "forbidden");
        return;
    }
    let form = parse_query(&String::from_utf8_lossy(&req.body));
    match req.path.as_str() {
        "/admin/provider/save" => {
            let mut providers = load_providers(cfg);
            let id = canonical_provider_id(
                &query_get(&form, "id").unwrap_or_else(|| "codex".to_string()),
            );
            let auth_path = query_get(&form, "auth_path")
                .unwrap_or_else(|| default_provider_auth_path(cfg, &id).display().to_string());
            let enabled = query_get(&form, "enabled").is_some();
            if let Some(provider) = providers
                .iter_mut()
                .find(|p| canonical_provider_id(&p.id) == id)
            {
                provider.enabled = enabled;
                provider.auth_path = PathBuf::from(auth_path);
                provider.id = id.clone();
                provider.name = provider_display_name(&id).to_string();
            } else {
                providers.push(Provider {
                    id: id.clone(),
                    name: provider_display_name(&id).to_string(),
                    enabled,
                    auth_path: PathBuf::from(auth_path),
                });
            }
            let _ = save_providers(cfg, &providers);
        }
        "/admin/users/save" => {
            let email = query_get(&form, "email").unwrap_or_default();
            if !email.trim().is_empty() {
                let name = query_get(&form, "name")
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| email.clone());
                let cost_share_pct = query_get(&form, "cost_share_pct")
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.0);
                let enabled = query_get(&form, "enabled").is_some();
                let _ = accounts::upsert_user(
                    cfg,
                    RouterUser {
                        email,
                        name,
                        enabled,
                        cost_share_pct,
                    },
                );
            }
        }
        "/admin/billing/save" => {
            let monthly_shared_cost_usd = query_get(&form, "monthly_shared_cost_usd")
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .unwrap_or(0.0)
                .max(0.0);
            let _ = accounts::save_billing_config(
                cfg,
                &BillingConfig {
                    monthly_shared_cost_usd,
                },
            );
        }
        "/admin/keys/create" => {
            let email = query_get(&form, "email").unwrap_or_default();
            let name = query_get(&form, "name").unwrap_or_else(|| "Router API key".to_string());
            match accounts::create_client_key(cfg, &email, &name) {
                Ok((key, plaintext)) => {
                    generated_key_page(stream, cfg, &session.email, &key.id, &plaintext);
                    return;
                }
                Err(err) => {
                    let _ = http::send_text(stream, 400, "text/plain", &err);
                    return;
                }
            }
        }
        "/admin/keys/revoke" => {
            if let Some(id) = query_get(&form, "id") {
                let enabled = query_get(&form, "enabled").as_deref() == Some("true");
                let _ = accounts::set_client_key_enabled(cfg, &id, enabled);
            }
        }
        "/admin/models/add" => {
            let id = query_get(&form, "id").unwrap_or_default();
            if !id.trim().is_empty() {
                let name = query_get(&form, "name")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| id.clone());
                let upstream_id = query_get(&form, "upstream_id")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| id.clone());
                let provider_id = query_get(&form, "provider_id")
                    .filter(|s| !s.is_empty())
                    .map(|s| canonical_provider_id(&s))
                    .unwrap_or_else(|| infer_model_provider_id(&id));
                let mut models = load_models(cfg);
                models.retain(|m| m.id != id);
                models.push(Model {
                    id,
                    name,
                    upstream_id,
                    provider_id,
                    enabled: true,
                });
                let _ = save_models(cfg, &models);
            }
        }
        "/admin/models/remove" => {
            if let Some(id) = query_get(&form, "id") {
                let mut models = load_models(cfg);
                models.retain(|m| m.id != id);
                let _ = save_models(cfg, &models);
            }
        }
        "/admin/models/sync" => {
            if let Ok(resp) = upstream::fetch_codex_models(cfg) {
                if resp.status == 200 {
                    merge_remote_models(cfg, &String::from_utf8_lossy(&resp.body), "codex");
                }
            }
            if let Ok(resp) = upstream::fetch_claude_models(cfg) {
                if resp.status == 200 {
                    merge_remote_models(cfg, &String::from_utf8_lossy(&resp.body), "claude");
                }
            }
            if let Ok(resp) = upstream::fetch_opencode_go_models(cfg) {
                if resp.status == 200 {
                    merge_remote_models(cfg, &String::from_utf8_lossy(&resp.body), "opencode-go");
                }
            }
        }
        _ => {}
    }
    let _ = http::redirect(stream, "/admin", &[]);
}

fn generated_key_page(
    stream: &mut TcpStream,
    cfg: &Config,
    email: &str,
    key_id: &str,
    plaintext: &str,
) {
    let html = format!(
        r#"<!doctype html>
<html lang="en" data-theme="">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>AkurAI Router Key</title><script>{flash_js}</script><style>{themes}{css}</style></head>
<body class="admin-body">
<header class="topbar"><a class="brand" href="/">AkurAI Router</a><nav><span>{email}</span><a href="/admin">Admin</a><span data-theme-picker></span></nav></header>
<main class="admin">
  <section class="panel wide">
    <p class="eyebrow">Router key created</p>
    <h1>{key_id}</h1>
    <p class="muted">This key is shown once. Store it in the client secret manager, then use it as `Authorization: Bearer ...` against `{base}/v1`.</p>
    <pre class="key-box">{key}</pre>
    <a class="button" href="/admin">Return to admin</a>
  </section>
</main>
<script>{picker_js}</script>
</body>
</html>"#,
        flash_js = flash_guard_js(),
        themes = themes_css(),
        css = style(),
        picker_js = theme_picker_js(),
        email = html_escape(email),
        key_id = html_escape(key_id),
        key = html_escape(plaintext),
        base = html_escape(&cfg.public_base_url),
    );
    let _ = http::send_text(stream, 200, "text/html; charset=utf-8", &html);
}

fn usage_summary_for(summaries: &[UsageSummary], email: &str) -> UsageSummary {
    summaries
        .iter()
        .find(|summary| summary.email.eq_ignore_ascii_case(email))
        .cloned()
        .unwrap_or_else(|| UsageSummary {
            email: email.to_string(),
            ..UsageSummary::default()
        })
}

fn money(value: f64) -> String {
    format!("${:.4}", value.max(0.0))
}

fn format_u64(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::new();
    for (idx, ch) in raw.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn merge_remote_models(cfg: &Config, text: &str, provider_id: &str) {
    let Ok(json) = crate::json::parse(text) else {
        return;
    };
    let Some(crate::json::Json::Array(remote)) = json.get("data").or_else(|| json.get("models"))
    else {
        return;
    };
    let provider_id = canonical_provider_id(provider_id);
    let mut models = load_models(cfg);
    for item in remote {
        let Some(id) = item.get_str("id").or_else(|| item.get_str("name")) else {
            continue;
        };
        let model_id = model_id_for_provider(id, &provider_id);
        if skip_remote_model(id) {
            continue;
        }
        if models
            .iter()
            .any(|m| canonical_provider_id(&m.provider_id) == provider_id && m.id == model_id)
        {
            continue;
        }
        models.push(Model {
            id: model_id.clone(),
            name: id.to_string(),
            upstream_id: model_id,
            provider_id: provider_id.clone(),
            enabled: true,
        });
    }
    let _ = save_models(cfg, &models);
}

fn skip_remote_model(id: &str) -> bool {
    id == "claude-fable-5"
}

pub fn logout(req: &Request, stream: &mut TcpStream, cfg: &Config) {
    if let Some(session) = auth::admin_session(req, cfg) {
        auth::remove_session(cfg, &session.token);
    }
    let _ = http::redirect(stream, "/", &[auth::clear_session_cookie(cfg)]);
}

// ── Theme CSS ─────────────────────────────────────────────────────────────────
// All 17 AkurAI-Framework base16 theme token blocks. Generated from the same
// scheme YAML files used by akurai-css::theme, following the identical
// slot→token mapping (base00→--bg … base0E→--accent-2). The first (akurai)
// is also emitted as the bare :root default so pages with no data-theme set
// render correctly. --radius is a layout constant set once in :root; it does
// not change per-theme. --shadow tracks polarity (dark/light).
fn themes_css() -> &'static str {
    r#":root{--bg:#060912;--bg-2:#0b1224;--panel:#0e162b;--panel-2:#1b2742;--border:#1b2742;--border-2:#9fb0d4;--fg:#eef3ff;--muted:#9fb0d4;--dim:#1b2742;--accent:#5b8cff;--accent-2:#7af0d0;--ok:#7af0d0;--warn:#ffcf8b;--info:#6bb8ff;--danger:#ff6b81;--radius:8px;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
:root[data-theme="akurai"]{--bg:#060912;--bg-2:#0b1224;--panel:#0e162b;--panel-2:#1b2742;--border:#1b2742;--border-2:#9fb0d4;--fg:#eef3ff;--muted:#9fb0d4;--dim:#1b2742;--accent:#5b8cff;--accent-2:#7af0d0;--ok:#7af0d0;--warn:#ffcf8b;--info:#6bb8ff;--danger:#ff6b81;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
:root[data-theme="akurai-light"]{--bg:#f7f8fb;--bg-2:#eef1f6;--panel:#e2e7f0;--panel-2:#cdd5e2;--border:#cdd5e2;--border-2:#5b657d;--fg:#1d2430;--muted:#5b657d;--dim:#cdd5e2;--accent:#2e56b8;--accent-2:#6d4bd8;--ok:#1f9d57;--warn:#b7791f;--info:#2570d4;--danger:#d92d20;--shadow:0 18px 45px -24px rgba(20,22,30,.16);color-scheme:light}
:root[data-theme="claude-code"]{--bg:#1f1e1b;--bg-2:#262320;--panel:#2f2b27;--panel-2:#3a352f;--border:#3a352f;--border-2:#8a8279;--fg:#e6e0d6;--muted:#8a8279;--dim:#3a352f;--accent:#cc785c;--accent-2:#a87b9e;--ok:#8a9a5b;--warn:#d9a05b;--info:#5b9aa0;--danger:#bf4d43;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
:root[data-theme="claude-code-light"]{--bg:#f5f1e8;--bg-2:#ece7da;--panel:#e0dac9;--panel-2:#cfc8b5;--border:#cfc8b5;--border-2:#6b6457;--fg:#3d3a34;--muted:#6b6457;--dim:#cfc8b5;--accent:#c15f3c;--accent-2:#8a5a7e;--ok:#5f7a3a;--warn:#b07a2e;--info:#3f7a80;--danger:#bf4d43;--shadow:0 18px 45px -24px rgba(20,22,30,.16);color-scheme:light}
:root[data-theme="nord"]{--bg:#2e3440;--bg-2:#3b4252;--panel:#434c5e;--panel-2:#4c566a;--border:#4c566a;--border-2:#d8dee9;--fg:#e5e9f0;--muted:#d8dee9;--dim:#4c566a;--accent:#81a1c1;--accent-2:#b48ead;--ok:#a3be8c;--warn:#ebcb8b;--info:#88c0d0;--danger:#bf616a;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
:root[data-theme="nord-light"]{--bg:#eceff4;--bg-2:#e5e9f0;--panel:#d8dee9;--panel-2:#aebacf;--border:#aebacf;--border-2:#4c566a;--fg:#2e3440;--muted:#4c566a;--dim:#aebacf;--accent:#5e81ac;--accent-2:#8a5a7e;--ok:#6e8b4f;--warn:#a9863a;--info:#2d7d8a;--danger:#b1444e;--shadow:0 18px 45px -24px rgba(20,22,30,.16);color-scheme:light}
:root[data-theme="catppuccin-mocha"]{--bg:#1e1e2e;--bg-2:#181825;--panel:#313244;--panel-2:#45475a;--border:#45475a;--border-2:#585b70;--fg:#cdd6f4;--muted:#585b70;--dim:#45475a;--accent:#89b4fa;--accent-2:#cba6f7;--ok:#a6e3a1;--warn:#f9e2af;--info:#94e2d5;--danger:#f38ba8;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
:root[data-theme="catppuccin-latte"]{--bg:#eff1f5;--bg-2:#e6e9ef;--panel:#ccd0da;--panel-2:#bcc0cc;--border:#bcc0cc;--border-2:#acb0be;--fg:#4c4f69;--muted:#acb0be;--dim:#bcc0cc;--accent:#1e66f5;--accent-2:#8839ef;--ok:#40a02b;--warn:#df8e1d;--info:#179299;--danger:#d20f39;--shadow:0 18px 45px -24px rgba(20,22,30,.16);color-scheme:light}
:root[data-theme="solarized-dark"]{--bg:#002b36;--bg-2:#073642;--panel:#586e75;--panel-2:#657b83;--border:#657b83;--border-2:#839496;--fg:#93a1a1;--muted:#839496;--dim:#657b83;--accent:#268bd2;--accent-2:#6c71c4;--ok:#859900;--warn:#b58900;--info:#2aa198;--danger:#dc322f;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
:root[data-theme="solarized-light"]{--bg:#fdf6e3;--bg-2:#eee8d5;--panel:#93a1a1;--panel-2:#839496;--border:#839496;--border-2:#657b83;--fg:#586e75;--muted:#657b83;--dim:#839496;--accent:#268bd2;--accent-2:#6c71c4;--ok:#859900;--warn:#b58900;--info:#2aa198;--danger:#dc322f;--shadow:0 18px 45px -24px rgba(20,22,30,.16);color-scheme:light}
:root[data-theme="gruvbox-dark"]{--bg:#282828;--bg-2:#3c3836;--panel:#504945;--panel-2:#665c54;--border:#665c54;--border-2:#bdae93;--fg:#d5c4a1;--muted:#bdae93;--dim:#665c54;--accent:#83a598;--accent-2:#d3869b;--ok:#b8bb26;--warn:#fabd2f;--info:#8ec07c;--danger:#fb4934;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
:root[data-theme="gruvbox-light"]{--bg:#fbf1c7;--bg-2:#ebdbb2;--panel:#d5c4a1;--panel-2:#bdae93;--border:#bdae93;--border-2:#665c54;--fg:#504945;--muted:#665c54;--dim:#bdae93;--accent:#076678;--accent-2:#8f3f71;--ok:#79740e;--warn:#b57614;--info:#427b58;--danger:#9d0006;--shadow:0 18px 45px -24px rgba(20,22,30,.16);color-scheme:light}
:root[data-theme="tokyo-night"]{--bg:#1a1b26;--bg-2:#16161e;--panel:#2f3549;--panel-2:#444b6a;--border:#444b6a;--border-2:#787c99;--fg:#a9b1d6;--muted:#787c99;--dim:#444b6a;--accent:#2ac3de;--accent-2:#bb9af7;--ok:#9ece6a;--warn:#0db9d7;--info:#b4f9f8;--danger:#c0caf5;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
:root[data-theme="tokyo-night-light"]{--bg:#d5d6db;--bg-2:#cbccd1;--panel:#dfe0e5;--panel-2:#9699a3;--border:#9699a3;--border-2:#4c505e;--fg:#343b59;--muted:#4c505e;--dim:#9699a3;--accent:#34548a;--accent-2:#5a4a78;--ok:#485e30;--warn:#166775;--info:#3e6968;--danger:#343b58;--shadow:0 18px 45px -24px rgba(20,22,30,.16);color-scheme:light}
:root[data-theme="rose-pine"]{--bg:#191724;--bg-2:#1f1d2e;--panel:#26233a;--panel-2:#6e6a86;--border:#6e6a86;--border-2:#908caa;--fg:#e0def4;--muted:#908caa;--dim:#6e6a86;--accent:#c4a7e7;--accent-2:#f6c177;--ok:#31748f;--warn:#ebbcba;--info:#9ccfd8;--danger:#eb6f92;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
:root[data-theme="rose-pine-dawn"]{--bg:#faf4ed;--bg-2:#fffaf3;--panel:#f2e9de;--panel-2:#9893a5;--border:#9893a5;--border-2:#797593;--fg:#575279;--muted:#797593;--dim:#9893a5;--accent:#907aa9;--accent-2:#ea9d34;--ok:#286983;--warn:#d7827e;--info:#56949f;--danger:#b4637a;--shadow:0 18px 45px -24px rgba(20,22,30,.16);color-scheme:light}
:root[data-theme="dracula"]{--bg:#282a36;--bg-2:#21222c;--panel:#44475a;--panel-2:#6272a4;--border:#6272a4;--border-2:#9ea8c7;--fg:#f8f8f2;--muted:#9ea8c7;--dim:#6272a4;--accent:#bd93f9;--accent-2:#ff79c6;--ok:#50fa7b;--warn:#f1fa8c;--info:#8be9fd;--danger:#ff5555;--shadow:0 24px 60px -28px rgba(0,0,0,.55);color-scheme:dark}
"#
}

// ── Flash guard ────────────────────────────────────────────────────────────────
// Runs synchronously in <head> before first paint. Reads the suite-wide
// akurai-theme cookie (written by any *.olibuijr.com app), falls back to
// localStorage, then OS colour-scheme preference. Sets data-theme on <html>
// so the correct :root[data-theme] token block applies immediately — no FOUC.
fn flash_guard_js() -> &'static str {
    r#"(function(){var m=document.cookie.match(/(?:^|; )akurai-theme=([^;]*)/);var t=m?decodeURIComponent(m[1]):null;if(!t){try{t=localStorage.getItem('akurai-theme');}catch(e){}}if(!t)t=(window.matchMedia&&window.matchMedia('(prefers-color-scheme:light)').matches)?'akurai-light':'akurai';document.documentElement.setAttribute('data-theme',t);})();"#
}

// ── Theme picker + suite sync ──────────────────────────────────────────────────
// Mounts a <select> into any [data-theme-picker] slot. Persists choice to the
// .olibuijr.com cookie so all suite apps (akurai-drive, framework, router …)
// share the same active theme. Syncs open tabs via BroadcastChannel and
// re-adopts the cookie on focus/visibilitychange for cross-subdomain live sync.
fn theme_picker_js() -> &'static str {
    r#"(function(){var KEY='akurai-theme';var THEMES=[['akurai','AkurAI'],['akurai-light','AkurAI Light'],['claude-code','Claude Code'],['claude-code-light','Claude Code Light'],['nord','Nord'],['nord-light','Nord Light'],['catppuccin-mocha','Catppuccin Mocha'],['catppuccin-latte','Catppuccin Latte'],['solarized-dark','Solarized Dark'],['solarized-light','Solarized Light'],['gruvbox-dark','Gruvbox Dark'],['gruvbox-light','Gruvbox Light'],['tokyo-night','Tokyo Night'],['tokyo-night-light','Tokyo Night Light'],['rose-pine','Rose Pine'],['rose-pine-dawn','Rose Pine Dawn'],['dracula','Dracula']];function pDom(){var h=location.hostname;if(!h||h==='localhost'||/^[0-9.]+$/.test(h))return '';var p=h.split('.');return p.length<2?'':'.'+p.slice(-2).join('.');}function readCookie(){var m=document.cookie.match(/(?:^|; )akurai-theme=([^;]*)/);return m?decodeURIComponent(m[1]):null;}function writeCookie(s){var dom=pDom(),sec=location.protocol==='https:'?';secure':'';document.cookie=KEY+'='+encodeURIComponent(s)+';path=/;max-age=31536000;samesite=lax'+(dom?';domain='+dom:'')+sec;}function reflect(s){if(!s)return;document.documentElement.setAttribute('data-theme',s);try{localStorage.setItem(KEY,s);}catch(e){}if(sel)sel.value=s;}function apply(s){reflect(s);writeCookie(s);try{ch&&ch.postMessage(s);}catch(e){}}function current(){return readCookie()||(function(){try{return localStorage.getItem(KEY);}catch(e){return null;}})()||(window.matchMedia&&window.matchMedia('(prefers-color-scheme:light)').matches?'akurai-light':'akurai');}var sel=null;var ch=null;try{ch='BroadcastChannel' in window?new BroadcastChannel('akurai-theme'):null;}catch(e){}if(ch)ch.onmessage=function(e){reflect(e.data);};var adoptCookie=function(){var c=readCookie();if(c)reflect(c);};window.addEventListener('focus',adoptCookie);document.addEventListener('visibilitychange',function(){if(document.visibilityState==='visible')adoptCookie();});var slot=document.querySelector('[data-theme-picker]');if(slot){sel=document.createElement('select');sel.className='theme-select';sel.setAttribute('aria-label','Color theme');THEMES.forEach(function(t){var o=document.createElement('option');o.value=t[0];o.textContent=t[1];sel.appendChild(o);});var c=current();sel.value=c;reflect(c);sel.addEventListener('change',function(){apply(sel.value);});slot.appendChild(sel);}else{reflect(current());}})();"#
}

// ── Component CSS ──────────────────────────────────────────────────────────────
// All layout, typography, and component rules expressed through the semantic
// tokens emitted by themes_css(). No hardcoded colour values — every colour
// reference is a var(--token). The admin page now inherits the same dark/light
// theming as the landing page; the previous hardcoded light-cream admin
// background (#f4f0e8) and white panels (#fff) are replaced by var(--bg) /
// var(--panel) so the admin matches whatever theme is active suite-wide.
fn style() -> &'static str {
    r#"*{box-sizing:border-box}body{margin:0;font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:var(--bg);color:var(--fg)}a{color:inherit;text-decoration:none}nav{position:absolute;top:0;left:0;right:0;z-index:3;display:flex;align-items:center;justify-content:space-between;padding:22px clamp(20px,5vw,64px)}nav div{display:flex;gap:18px;align-items:center}.brand{font-weight:760;letter-spacing:0}.hero{position:relative;min-height:92vh;overflow:hidden;display:flex;align-items:center}.hero-img{position:absolute;inset:0;width:100%;height:100%;object-fit:cover}.shade{position:absolute;inset:0;background:linear-gradient(90deg,rgba(0,0,0,.82) 0%,rgba(0,0,0,.56) 38%,rgba(0,0,0,.18) 70%,rgba(0,0,0,.64) 100%)}.copy{position:relative;z-index:2;width:min(720px,92vw);margin-left:clamp(22px,6vw,86px);padding-top:36px}.eyebrow{font-size:13px;text-transform:uppercase;letter-spacing:.12em;color:var(--accent-2);font-weight:800}.copy h1{font-size:clamp(56px,8vw,120px);line-height:.92;margin:12px 0 22px;letter-spacing:0}.lead{font-size:clamp(18px,2vw,24px);line-height:1.5;color:var(--fg);opacity:.85;max-width:680px}.actions{display:flex;gap:12px;flex-wrap:wrap;margin-top:30px}.button,button{display:inline-flex;align-items:center;justify-content:center;min-height:42px;border:1px solid var(--accent);background:var(--accent);color:var(--bg);border-radius:var(--radius);padding:0 16px;font-weight:760;cursor:pointer;font:inherit}.button.secondary,.button.ghost{background:var(--panel);color:var(--fg);border-color:var(--border)}.band{background:var(--bg-2);color:var(--fg);padding:32px clamp(20px,5vw,64px) 64px}.grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:24px;max-width:1120px;margin:auto}.grid h2{font-size:20px;margin:0 0 8px}.grid p{margin:0;line-height:1.55;color:var(--muted)}.admin-body{background:var(--bg);color:var(--fg)}.topbar{display:flex;justify-content:space-between;align-items:center;padding:16px 24px;background:var(--bg-2);color:var(--fg);border-bottom:1px solid var(--border)}.topbar nav{position:static;padding:0;gap:16px}.topbar a{color:var(--muted)}.topbar a:hover{color:var(--fg)}.admin{padding:24px;display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:18px}.panel{background:var(--panel);border:1px solid var(--border);border-radius:var(--radius);padding:20px;box-shadow:var(--shadow)}.panel.wide{grid-column:1/-1}.panel h1{font-size:24px;margin:2px 0 0;color:var(--fg);overflow-wrap:anywhere}.panel h2{font-size:18px;margin:0 0 14px}.muted,.steps{color:var(--muted);line-height:1.5}.status-row,.row-head{display:flex;justify-content:space-between;gap:12px;align-items:center}.metric-grid{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:10px;margin-top:16px}.metric{border:1px solid var(--border);border-radius:var(--radius);padding:12px;background:var(--bg-2)}.metric span{display:block;color:var(--muted);font-size:13px}.metric strong{display:block;font-size:22px;margin-top:4px}.pill{display:inline-flex;border-radius:99px;padding:6px 10px;font-size:13px;font-weight:780}.pill.ok{background:color-mix(in srgb,var(--ok) 18%,var(--panel));color:var(--ok)}.pill.warn{background:color-mix(in srgb,var(--warn) 18%,var(--panel));color:var(--warn)}.provider-list{display:grid;gap:12px}.provider-form{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1.3fr) auto auto auto;gap:10px;align-items:end;padding:12px;border:1px solid var(--border);border-radius:var(--radius);background:var(--bg-2)}.provider-form strong{display:block;font-size:15px}.provider-main{display:grid;gap:4px;align-self:start}.stack{display:grid;gap:12px}.stack label,.provider-form label{display:grid;gap:6px;font-weight:700}.stack input,.inline-form input,.provider-form input,.mini-form input,.inline-form select{height:38px;border:1px solid var(--border);border-radius:6px;padding:0 10px;font:inherit;background:var(--bg);color:var(--fg)}.check{display:flex!important;grid-template-columns:auto 1fr;align-items:center}.check input{height:auto}.inline-form{display:grid;grid-template-columns:repeat(4,minmax(0,1fr)) auto;gap:8px;margin-top:14px}.key-add{grid-template-columns:minmax(0,1fr) minmax(0,1fr) auto}.user-add{grid-template-columns:minmax(0,1.2fr) minmax(0,1fr) minmax(120px,.5fr) auto auto}.mini-form{display:flex;gap:8px;align-items:end}.mini-form label{display:grid;gap:5px;font-size:13px;font-weight:700}.key-box{white-space:pre-wrap;overflow-wrap:anywhere;background:var(--bg);color:var(--accent-2);border:1px solid var(--border);border-radius:var(--radius);padding:14px;font-size:15px}table{width:100%;border-collapse:collapse;font-size:14px}td,th{border-bottom:1px solid var(--border);padding:10px;text-align:left}th{color:var(--muted)}.icon{min-height:30px;width:32px;padding:0;background:var(--panel);color:var(--danger);border-color:color-mix(in srgb,var(--danger) 30%,var(--border))}.theme-select{height:32px;padding:0 8px;border-radius:6px;border:1px solid var(--border);background:var(--panel);color:var(--fg);font:inherit;font-size:13px;cursor:pointer}@media(max-width:820px){.grid,.admin,.provider-form,.metric-grid{grid-template-columns:1fr}.copy h1{font-size:54px}.inline-form,.key-add,.user-add{grid-template-columns:1fr}.status-row,.row-head,.mini-form{display:grid}.topbar{display:grid;gap:10px}.hero{min-height:88vh}}
"#
}
