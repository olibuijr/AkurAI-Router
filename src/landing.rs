use std::net::TcpStream;
use std::path::PathBuf;

use crate::auth;
use crate::config::{
    Config, Model, Provider, load_models, load_providers, save_models, save_providers,
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
  <meta name="description" content="AkurAI Router is a std-only Rust OpenAI-compatible endpoint for routing requests to Codex OAuth.">
  <meta property="og:title" content="AkurAI Router">
  <meta property="og:description" content="A minimal Rust OpenAI-compatible router for Codex OAuth.">
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
      <p class="eyebrow">OpenAI endpoint. Codex OAuth upstream.</p>
      <h1>AkurAI Router</h1>
      <p class="lead">A small Rust service that exposes `/v1/responses` and `/v1/models`, authenticates tools with a private API key, and routes requests through the Codex CLI OAuth identity on the VM.</p>
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
      <div><h2>Codex native</h2><p>Requests are normalized for the Codex Responses endpoint with Codex CLI identity headers and OAuth token refresh.</p></div>
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
    let providers = load_providers(cfg);
    let models = load_models(cfg);
    let provider = providers
        .iter()
        .find(|p| p.id == "codex")
        .cloned()
        .unwrap_or(Provider {
            id: "codex".to_string(),
            name: "OpenAI Codex".to_string(),
            enabled: true,
            auth_path: cfg.codex_auth_path.clone(),
        });
    let auth_ok = provider.auth_path.exists();
    let model_rows = models
        .iter()
        .map(|m| {
            format!(
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><form method="post" action="/admin/models/remove"><input type="hidden" name="id" value="{}"><button class="icon" title="Remove">×</button></form></td></tr>"#,
                html_escape(&m.id),
                html_escape(&m.name),
                html_escape(&m.upstream_id),
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
  </section>

  <section class="panel">
    <h2>Provider</h2>
    <form method="post" action="/admin/provider/save" class="stack">
      <label>Codex auth path<input name="auth_path" value="{auth_path}"></label>
      <label>Default model<input name="default_model" value="{default_model}" disabled></label>
      <label class="check"><input type="checkbox" name="enabled" {checked}> Enabled</label>
      <button>Save provider</button>
    </form>
    <p class="muted">{provider_hint}</p>
  </section>

  <section class="panel">
    <h2>Login Setup</h2>
    <ol class="steps">
      <li>SSH to the VM as `ubuntu` and run `codex login` when the Codex OAuth token needs renewal.</li>
      <li>Keep `~/.codex/auth.json` readable only by the service account.</li>
      <li>Configure clients with base URL `{base}/v1`, wire API `responses`, and the router API key from `/etc/akurai-router/router.env`.</li>
    </ol>
  </section>

  <section class="panel wide">
    <div class="row-head"><h2>Models</h2><form method="post" action="/admin/models/sync"><button>Sync Codex catalog</button></form></div>
    <table><thead><tr><th>ID</th><th>Name</th><th>Upstream</th><th>Status</th><th></th></tr></thead><tbody>{model_rows}</tbody></table>
    <form method="post" action="/admin/models/add" class="inline-form">
      <input name="id" placeholder="model id">
      <input name="name" placeholder="display name">
      <input name="upstream_id" placeholder="upstream id">
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
            "Codex auth found"
        } else {
            "Codex auth missing"
        },
        auth_path = html_escape(&provider.auth_path.display().to_string()),
        default_model = html_escape(&cfg.default_model),
        checked = if provider.enabled { "checked" } else { "" },
        provider_hint = if auth_ok {
            "The router will use the Codex OAuth material at this path and refresh access tokens when possible."
        } else {
            "Run codex login on the VM or set AKURAI_ROUTER_CODEX_AUTH_PATH to the correct auth.json path."
        },
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
            let auth_path = query_get(&form, "auth_path")
                .unwrap_or_else(|| cfg.codex_auth_path.display().to_string());
            let enabled = query_get(&form, "enabled").is_some();
            if let Some(provider) = providers.iter_mut().find(|p| p.id == "codex") {
                provider.enabled = enabled;
                provider.auth_path = PathBuf::from(auth_path);
            } else {
                providers.push(Provider {
                    id: "codex".to_string(),
                    name: "OpenAI Codex".to_string(),
                    enabled,
                    auth_path: PathBuf::from(auth_path),
                });
            }
            let _ = save_providers(cfg, &providers);
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
                let mut models = load_models(cfg);
                models.retain(|m| m.id != id);
                models.push(Model {
                    id,
                    name,
                    upstream_id,
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
                    merge_remote_models(cfg, &String::from_utf8_lossy(&resp.body));
                }
            }
        }
        _ => {}
    }
    let _ = http::redirect(stream, "/admin", &[]);
}

fn merge_remote_models(cfg: &Config, text: &str) {
    let Ok(json) = crate::json::parse(text) else {
        return;
    };
    let Some(crate::json::Json::Array(remote)) = json.get("data").or_else(|| json.get("models"))
    else {
        return;
    };
    let mut models = load_models(cfg);
    for item in remote {
        let Some(id) = item.get_str("id").or_else(|| item.get_str("name")) else {
            continue;
        };
        if models.iter().any(|m| m.id == id) {
            continue;
        }
        models.push(Model {
            id: id.to_string(),
            name: id.to_string(),
            upstream_id: id.to_string(),
            enabled: true,
        });
    }
    let _ = save_models(cfg, &models);
}

pub fn logout(req: &Request, stream: &mut TcpStream, cfg: &Config) {
    if let Some(session) = auth::admin_session(req, cfg) {
        auth::remove_session(cfg, &session.token);
    }
    let _ = http::redirect(stream, "/", &[auth::clear_session_cookie(cfg)]);
}

fn style() -> &'static str {
    r#"
*{box-sizing:border-box}body{margin:0;font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#101312;color:#f7f3eb}a{color:inherit;text-decoration:none}nav{position:absolute;top:0;left:0;right:0;z-index:3;display:flex;align-items:center;justify-content:space-between;padding:22px clamp(20px,5vw,64px)}nav div{display:flex;gap:18px;align-items:center}.brand{font-weight:760;letter-spacing:0}.hero{position:relative;min-height:92vh;overflow:hidden;display:flex;align-items:center}.hero-img{position:absolute;inset:0;width:100%;height:100%;object-fit:cover}.shade{position:absolute;inset:0;background:linear-gradient(90deg,rgba(12,14,14,.82) 0%,rgba(12,14,14,.64) 38%,rgba(12,14,14,.24) 70%,rgba(12,14,14,.72) 100%)}.copy{position:relative;z-index:2;width:min(720px,92vw);margin-left:clamp(22px,6vw,86px);padding-top:36px}.eyebrow{font-size:13px;text-transform:uppercase;letter-spacing:.12em;color:#6ee7c8;font-weight:800}.copy h1{font-size:clamp(56px,8vw,120px);line-height:.92;margin:12px 0 22px;letter-spacing:0}.lead{font-size:clamp(18px,2vw,24px);line-height:1.5;color:#ece4d5;max-width:680px}.actions{display:flex;gap:12px;flex-wrap:wrap;margin-top:30px}.button,button{display:inline-flex;align-items:center;justify-content:center;min-height:42px;border:1px solid #6ee7c8;background:#6ee7c8;color:#08110f;border-radius:7px;padding:0 16px;font-weight:760;cursor:pointer}.button.secondary,.button.ghost{background:rgba(255,255,255,.08);color:#f7f3eb;border-color:rgba(255,255,255,.22)}.band{background:#f7f3eb;color:#141817;padding:32px clamp(20px,5vw,64px) 64px}.grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:24px;max-width:1120px;margin:auto}.grid h2{font-size:20px;margin:0 0 8px}.grid p{margin:0;line-height:1.55;color:#3f4744}.admin-body{background:#f4f0e8;color:#151a18}.topbar{display:flex;justify-content:space-between;align-items:center;padding:16px 24px;background:#101312;color:#f7f3eb}.topbar nav{position:static;padding:0;gap:18px}.admin{padding:24px;display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:18px}.panel{background:#fff;border:1px solid #d8d1c5;border-radius:8px;padding:20px;box-shadow:0 1px 2px rgba(0,0,0,.04)}.panel.wide{grid-column:1/-1}.panel h1{font-size:24px;margin:2px 0 0;color:#151a18;overflow-wrap:anywhere}.panel h2{font-size:18px;margin:0 0 14px}.muted,.steps{color:#56605c;line-height:1.5}.status-row,.row-head{display:flex;justify-content:space-between;gap:12px;align-items:center}.pill{display:inline-flex;border-radius:99px;padding:6px 10px;font-size:13px;font-weight:780}.pill.ok{background:#d8f7e7;color:#075d39}.pill.warn{background:#fff1c2;color:#755400}.stack{display:grid;gap:12px}.stack label{display:grid;gap:6px;font-weight:700}.stack input,.inline-form input{height:38px;border:1px solid #cfc7ba;border-radius:6px;padding:0 10px;font:inherit}.check{display:flex!important;grid-template-columns:auto 1fr;align-items:center}.check input{height:auto}.inline-form{display:grid;grid-template-columns:repeat(3,minmax(0,1fr)) auto;gap:8px;margin-top:14px}table{width:100%;border-collapse:collapse;font-size:14px}td,th{border-bottom:1px solid #e6dfd4;padding:10px;text-align:left}th{color:#56605c}.icon{min-height:30px;width:32px;padding:0;background:#fff;color:#8a1f11;border-color:#e8c6be}@media(max-width:820px){.grid,.admin{grid-template-columns:1fr}.copy h1{font-size:54px}.inline-form{grid-template-columns:1fr}.status-row,.row-head{display:grid}.topbar{display:grid;gap:10px}.hero{min-height:88vh}}
"#
}
