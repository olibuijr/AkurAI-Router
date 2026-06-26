use std::env;
use std::path::PathBuf;

use crate::auth;
use crate::config::{
    Config, Model, Provider, ensure_default_files, load_models, load_providers, save_models,
    save_providers, write_local_env_template,
};
use crate::http::{self, Request};
use crate::util::{env_quote, random_hex};
use crate::{landing, oauth, upstream};

pub fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("serve");
    match cmd {
        "serve" => serve(),
        "init" => init(),
        "key" => key(&args[2..]),
        "providers" => providers(&args[2..]),
        "models" => models(&args[2..]),
        "idp" => idp(&args[2..]),
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("akurai-router {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(format!(
            "unknown command `{other}`; run `akurai-router help`"
        )),
    }
}

fn serve() -> Result<(), String> {
    let cfg = Config::load()?;
    ensure_default_files(&cfg)?;
    cfg.validate_for_serve()?;
    let addr = cfg.listen_addr.clone();
    http::serve(&addr, move |req, stream| dispatch(req, stream, &cfg))
}

fn dispatch(req: Request, stream: &mut std::net::TcpStream, cfg: &Config) {
    if req.method == "OPTIONS" {
        let _ = http::no_content(stream);
        return;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => landing::landing(&req, stream, cfg),
        ("GET", "/assets/hero.png") => landing::hero(stream),
        ("GET", "/health") => {
            let _ = http::send_json(stream, 200, "{\"ok\":true,\"service\":\"akurai-router\"}");
        }
        ("GET", "/login") => oauth::login(&req, stream, cfg),
        ("GET", "/auth/callback") => oauth::callback(&req, stream, cfg),
        ("GET", "/logout") => landing::logout(&req, stream, cfg),
        ("GET", "/admin") => landing::admin(&req, stream, cfg),
        ("POST", p) if p.starts_with("/admin/") => landing::admin_post(&req, stream, cfg),
        ("GET", "/v1/models") | ("GET", "/api/v1/models") => {
            if !auth::check_api_key(&req, cfg) {
                let _ =
                    http::send_json(stream, 401, "{\"error\":{\"message\":\"invalid API key\"}}");
                return;
            }
            let _ = http::send_json(stream, 200, &upstream::models_json(cfg));
        }
        ("POST", "/v1/responses")
        | ("POST", "/api/v1/responses")
        | ("POST", "/responses")
        | ("POST", "/codex")
        | ("POST", "/v1/chat/completions")
        | ("POST", "/api/v1/chat/completions") => {
            if !auth::check_api_key(&req, cfg) {
                let _ =
                    http::send_json(stream, 401, "{\"error\":{\"message\":\"invalid API key\"}}");
                return;
            }
            upstream::forward_model(&req, stream, cfg);
        }
        _ => {
            let _ = http::send_text(stream, 404, "text/plain", "not found");
        }
    }
}

fn init() -> Result<(), String> {
    let cfg = Config::load()?;
    let path = write_local_env_template(&cfg)?;
    ensure_default_files(&cfg)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn key(args: &[String]) -> Result<(), String> {
    match args.first().map(|s| s.as_str()) {
        Some("generate") | None => {
            println!("akr_{}", random_hex(32));
            Ok(())
        }
        _ => Err("usage: akurai-router key generate".to_string()),
    }
}

fn providers(args: &[String]) -> Result<(), String> {
    let cfg = Config::load()?;
    ensure_default_files(&cfg)?;
    match args.first().map(|s| s.as_str()) {
        Some("list") | None => {
            for p in load_providers(&cfg) {
                println!(
                    "{}\t{}\t{}\t{}",
                    p.id,
                    p.name,
                    if p.enabled { "enabled" } else { "disabled" },
                    p.auth_path.display()
                );
            }
            Ok(())
        }
        Some("add") => {
            let id = args.get(1).map(|s| s.as_str()).unwrap_or("codex");
            if id != "codex" && id != "claude" {
                return Err("provider id must be codex or claude".to_string());
            }
            let auth_path = args
                .iter()
                .position(|s| s == "--auth-path")
                .and_then(|i| args.get(i + 1))
                .map(PathBuf::from)
                .unwrap_or_else(|| match id {
                    "claude" => cfg.claude_auth_path.clone(),
                    _ => cfg.codex_auth_path.clone(),
                });
            let mut providers = load_providers(&cfg);
            providers.retain(|p| p.id != id);
            providers.push(Provider {
                id: id.to_string(),
                name: if id == "claude" {
                    "Claude Code".to_string()
                } else {
                    "OpenAI Codex".to_string()
                },
                enabled: true,
                auth_path,
            });
            save_providers(&cfg, &providers)
        }
        Some("disable") => set_provider_enabled(&cfg, false, args.get(1).map(|s| s.as_str())),
        Some("enable") => set_provider_enabled(&cfg, true, args.get(1).map(|s| s.as_str())),
        _ => Err(
            "usage: akurai-router providers [list|add codex|claude --auth-path PATH|enable [ID]|disable [ID]]"
                .to_string(),
        ),
    }
}

fn set_provider_enabled(
    cfg: &Config,
    enabled: bool,
    provider_id: Option<&str>,
) -> Result<(), String> {
    let mut providers = load_providers(cfg);
    let target = provider_id.unwrap_or("codex");
    for provider in &mut providers {
        if provider.id == target {
            provider.enabled = enabled;
        }
    }
    save_providers(cfg, &providers)
}

fn models(args: &[String]) -> Result<(), String> {
    let cfg = Config::load()?;
    ensure_default_files(&cfg)?;
    match args.first().map(|s| s.as_str()) {
        Some("list") | None => {
            for m in load_models(&cfg) {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    m.id,
                    m.name,
                    m.upstream_id,
                    m.provider_id,
                    if m.enabled { "enabled" } else { "disabled" }
                );
            }
            Ok(())
        }
        Some("add") => {
            let id = args.get(1).ok_or_else(|| "model id required".to_string())?.clone();
            let name = args.get(2).cloned().unwrap_or_else(|| id.clone());
            let upstream_id = args.get(3).cloned().unwrap_or_else(|| id.clone());
            let provider_id = args.get(4).cloned().unwrap_or_else(|| {
                if id.starts_with("claude-") || id.starts_with("cc/claude-") {
                    "claude".to_string()
                } else {
                    "codex".to_string()
                }
            });
            let mut models = load_models(&cfg);
            models.retain(|m| m.id != id);
            models.push(Model {
                id,
                name,
                upstream_id,
                provider_id,
                enabled: true,
            });
            save_models(&cfg, &models)
        }
        Some("remove") => {
            let id = args.get(1).ok_or_else(|| "model id required".to_string())?;
            let mut models = load_models(&cfg);
            models.retain(|m| &m.id != id);
            save_models(&cfg, &models)
        }
        Some("enable") | Some("disable") => {
            let enabled = args[0] == "enable";
            let id = args.get(1).ok_or_else(|| "model id required".to_string())?;
            let mut models = load_models(&cfg);
            for m in &mut models {
                if &m.id == id {
                    m.enabled = enabled;
                }
            }
            save_models(&cfg, &models)
        }
        _ => Err("usage: akurai-router models [list|add ID [NAME] [UPSTREAM] [PROVIDER]|remove ID|enable ID|disable ID]".to_string()),
    }
}

fn idp(args: &[String]) -> Result<(), String> {
    let cfg = Config::load()?;
    match args.first().map(|s| s.as_str()) {
        Some("client-json") | None => {
            println!(
                "{{\"name\":\"AkurAI Router\",\"tenant_id\":\"<tenant-id>\",\"redirect_uris\":[\"{}\"],\"grant_types\":[\"authorization_code\",\"refresh_token\"],\"scopes\":[\"openid\",\"profile\",\"email\",\"groups\"],\"first_party\":true}}",
                cfg.callback_url()
            );
            Ok(())
        }
        Some("env") => {
            println!("AKURAI_ROUTER_IDP_ISSUER={}", env_quote(&cfg.idp_issuer));
            println!(
                "AKURAI_ROUTER_IDP_CLIENT_ID={}",
                env_quote(&cfg.idp_client_id)
            );
            println!(
                "AKURAI_ROUTER_IDP_CLIENT_SECRET={}",
                env_quote(&cfg.idp_client_secret)
            );
            println!(
                "AKURAI_ROUTER_ADMIN_EMAIL={}",
                env_quote(&cfg.admin_allowed_email)
            );
            Ok(())
        }
        _ => Err("usage: akurai-router idp [client-json|env]".to_string()),
    }
}

fn print_help() {
    println!(
        r#"akurai-router {}

Usage:
  akurai-router serve
  akurai-router init
  akurai-router key generate
  akurai-router providers list
  akurai-router providers add codex --auth-path ~/.codex/auth.json
  akurai-router providers add claude --auth-path ~/.claude/.credentials.json
  akurai-router providers enable [ID]
  akurai-router providers disable [ID]
  akurai-router models list
  akurai-router models add ID [NAME] [UPSTREAM_ID] [PROVIDER_ID]
  akurai-router models remove|enable|disable ID
  akurai-router idp client-json

Environment:
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
"#,
        env!("CARGO_PKG_VERSION")
    );
}
