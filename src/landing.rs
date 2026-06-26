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
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="description" content="AkurAI Router is a std-only Rust OpenAI-compatible endpoint for Codex, Claude Code, and OpenCode Go routing.">
  <meta property="og:title" content="AkurAI Router">
  <meta property="og:description" content="A minimal Rust OpenAI-compatible router with provider-prefixed model routing.">
  <meta property="og:image" content="{base}/assets/hero.png">
  <title>AkurAI Router</title>
  <style>{css}</style>
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
</body>
</html>"#,
        base = cfg.public_base_url,
        css = style(),
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
<html lang="en">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>AkurAI Router Admin</title><style>{css}</style></head>
<body class="admin-body">
<header class="topbar"><a class="brand" href="/">AkurAI Router</a><nav><span>{email}</span><a href="/logout">Log out</a></nav></header>
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
</body>
</html>"#,
        css = style(),
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
<html lang="en">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>AkurAI Router Key</title><style>{css}</style></head>
<body class="admin-body">
<header class="topbar"><a class="brand" href="/">AkurAI Router</a><nav><span>{email}</span><a href="/admin">Admin</a></nav></header>
<main class="admin">
  <section class="panel wide">
    <p class="eyebrow">Router key created</p>
    <h1>{key_id}</h1>
    <p class="muted">This key is shown once. Store it in the client secret manager, then use it as `Authorization: Bearer ...` against `{base}/v1`.</p>
    <pre class="key-box">{key}</pre>
    <a class="button" href="/admin">Return to admin</a>
  </section>
</main>
</body>
</html>"#,
        css = style(),
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

fn style() -> &'static str {
    r#"
*{box-sizing:border-box}body{margin:0;font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#101312;color:#f7f3eb}a{color:inherit;text-decoration:none}nav{position:absolute;top:0;left:0;right:0;z-index:3;display:flex;align-items:center;justify-content:space-between;padding:22px clamp(20px,5vw,64px)}nav div{display:flex;gap:18px;align-items:center}.brand{font-weight:760;letter-spacing:0}.hero{position:relative;min-height:92vh;overflow:hidden;display:flex;align-items:center}.hero-img{position:absolute;inset:0;width:100%;height:100%;object-fit:cover}.shade{position:absolute;inset:0;background:linear-gradient(90deg,rgba(12,14,14,.82) 0%,rgba(12,14,14,.64) 38%,rgba(12,14,14,.24) 70%,rgba(12,14,14,.72) 100%)}.copy{position:relative;z-index:2;width:min(720px,92vw);margin-left:clamp(22px,6vw,86px);padding-top:36px}.eyebrow{font-size:13px;text-transform:uppercase;letter-spacing:.12em;color:#6ee7c8;font-weight:800}.copy h1{font-size:clamp(56px,8vw,120px);line-height:.92;margin:12px 0 22px;letter-spacing:0}.lead{font-size:clamp(18px,2vw,24px);line-height:1.5;color:#ece4d5;max-width:680px}.actions{display:flex;gap:12px;flex-wrap:wrap;margin-top:30px}.button,button{display:inline-flex;align-items:center;justify-content:center;min-height:42px;border:1px solid #6ee7c8;background:#6ee7c8;color:#08110f;border-radius:7px;padding:0 16px;font-weight:760;cursor:pointer}.button.secondary,.button.ghost{background:rgba(255,255,255,.08);color:#f7f3eb;border-color:rgba(255,255,255,.22)}.band{background:#f7f3eb;color:#141817;padding:32px clamp(20px,5vw,64px) 64px}.grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:24px;max-width:1120px;margin:auto}.grid h2{font-size:20px;margin:0 0 8px}.grid p{margin:0;line-height:1.55;color:#3f4744}.admin-body{background:#f4f0e8;color:#151a18}.topbar{display:flex;justify-content:space-between;align-items:center;padding:16px 24px;background:#101312;color:#f7f3eb}.topbar nav{position:static;padding:0;gap:18px}.admin{padding:24px;display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:18px}.panel{background:#fff;border:1px solid #d8d1c5;border-radius:8px;padding:20px;box-shadow:0 1px 2px rgba(0,0,0,.04)}.panel.wide{grid-column:1/-1}.panel h1{font-size:24px;margin:2px 0 0;color:#151a18;overflow-wrap:anywhere}.panel h2{font-size:18px;margin:0 0 14px}.muted,.steps{color:#56605c;line-height:1.5}.status-row,.row-head{display:flex;justify-content:space-between;gap:12px;align-items:center}.metric-grid{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:10px;margin-top:16px}.metric{border:1px solid #e6dfd4;border-radius:8px;padding:12px;background:#fcfbf8}.metric span{display:block;color:#56605c;font-size:13px}.metric strong{display:block;font-size:22px;margin-top:4px}.pill{display:inline-flex;border-radius:99px;padding:6px 10px;font-size:13px;font-weight:780}.pill.ok{background:#d8f7e7;color:#075d39}.pill.warn{background:#fff1c2;color:#755400}.provider-list{display:grid;gap:12px}.provider-form{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1.3fr) auto auto auto;gap:10px;align-items:end;padding:12px;border:1px solid #e6dfd4;border-radius:8px;background:#fcfbf8}.provider-form strong{display:block;font-size:15px}.provider-main{display:grid;gap:4px;align-self:start}.stack{display:grid;gap:12px}.stack label,.provider-form label{display:grid;gap:6px;font-weight:700}.stack input,.inline-form input,.provider-form input,.mini-form input,.inline-form select{height:38px;border:1px solid #cfc7ba;border-radius:6px;padding:0 10px;font:inherit;background:#fff}.check{display:flex!important;grid-template-columns:auto 1fr;align-items:center}.check input{height:auto}.inline-form{display:grid;grid-template-columns:repeat(4,minmax(0,1fr)) auto;gap:8px;margin-top:14px}.key-add{grid-template-columns:minmax(0,1fr) minmax(0,1fr) auto}.user-add{grid-template-columns:minmax(0,1.2fr) minmax(0,1fr) minmax(120px,.5fr) auto auto}.mini-form{display:flex;gap:8px;align-items:end}.mini-form label{display:grid;gap:5px;font-size:13px;font-weight:700}.key-box{white-space:pre-wrap;overflow-wrap:anywhere;background:#101312;color:#6ee7c8;border-radius:8px;padding:14px;font-size:15px}table{width:100%;border-collapse:collapse;font-size:14px}td,th{border-bottom:1px solid #e6dfd4;padding:10px;text-align:left}th{color:#56605c}.icon{min-height:30px;width:32px;padding:0;background:#fff;color:#8a1f11;border-color:#e8c6be}@media(max-width:820px){.grid,.admin,.provider-form,.metric-grid{grid-template-columns:1fr}.copy h1{font-size:54px}.inline-form,.key-add,.user-add{grid-template-columns:1fr}.status-row,.row-head,.mini-form{display:grid}.topbar{display:grid;gap:10px}.hero{min-height:88vh}}
"#
}
