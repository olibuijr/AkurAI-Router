use std::env;
use std::fs;
use std::path::PathBuf;

use crate::util::{env_quote, random_hex};

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_addr: String,
    pub public_base_url: String,
    pub data_dir: PathBuf,
    pub api_key: String,
    pub codex_auth_path: PathBuf,
    pub codex_responses_url: String,
    pub codex_models_url: String,
    pub claude_auth_path: PathBuf,
    pub claude_messages_url: String,
    pub claude_models_url: String,
    pub default_model: String,
    pub idp_issuer: String,
    pub idp_client_id: String,
    pub idp_client_secret: String,
    pub admin_allowed_email: String,
    pub cookie_secret: String,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub upstream_id: String,
    pub provider_id: String,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub auth_path: PathBuf,
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let data_dir = env::var("AKURAI_ROUTER_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".akurai-router")
            });
        fs::create_dir_all(&data_dir).map_err(|e| format!("failed to create data dir: {e}"))?;

        let conf = read_kv(&data_dir.join("router.conf"));
        let get = |key: &str, default: &str| -> String {
            env::var(key)
                .ok()
                .or_else(|| conf.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
                .unwrap_or_else(|| default.to_string())
        };

        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let api_key = get("AKURAI_ROUTER_API_KEY", "");
        let cookie_secret = get("AKURAI_ROUTER_COOKIE_SECRET", "");
        Ok(Config {
            listen_addr: get("AKURAI_ROUTER_LISTEN", "127.0.0.1:4219"),
            public_base_url: get("AKURAI_ROUTER_PUBLIC_URL", "http://127.0.0.1:4219")
                .trim_end_matches('/')
                .to_string(),
            data_dir,
            api_key,
            codex_auth_path: PathBuf::from(get(
                "AKURAI_ROUTER_CODEX_AUTH_PATH",
                &format!("{home}/.codex/auth.json"),
            )),
            codex_responses_url: get(
                "AKURAI_ROUTER_CODEX_RESPONSES_URL",
                "https://chatgpt.com/backend-api/codex/responses",
            ),
            codex_models_url: get(
                "AKURAI_ROUTER_CODEX_MODELS_URL",
                "https://chatgpt.com/backend-api/codex/models?client_version=1.0.0",
            ),
            claude_auth_path: PathBuf::from(get(
                "AKURAI_ROUTER_CLAUDE_AUTH_PATH",
                &format!("{home}/.claude/.credentials.json"),
            )),
            claude_messages_url: get(
                "AKURAI_ROUTER_CLAUDE_MESSAGES_URL",
                "https://api.anthropic.com/v1/messages",
            ),
            claude_models_url: get(
                "AKURAI_ROUTER_CLAUDE_MODELS_URL",
                "https://api.anthropic.com/v1/models",
            ),
            default_model: get("AKURAI_ROUTER_DEFAULT_MODEL", "gpt-5.4-mini"),
            idp_issuer: get("AKURAI_ROUTER_IDP_ISSUER", "https://auth.olibuijr.com")
                .trim_end_matches('/')
                .to_string(),
            idp_client_id: get("AKURAI_ROUTER_IDP_CLIENT_ID", ""),
            idp_client_secret: get("AKURAI_ROUTER_IDP_CLIENT_SECRET", ""),
            admin_allowed_email: get("AKURAI_ROUTER_ADMIN_EMAIL", "olibuijr@olibuijr.com"),
            cookie_secret,
        })
    }

    pub fn callback_url(&self) -> String {
        format!("{}/auth/callback", self.public_base_url)
    }

    pub fn validate_for_serve(&self) -> Result<(), String> {
        if self.api_key.is_empty() {
            return Err("AKURAI_ROUTER_API_KEY is required for API endpoints".to_string());
        }
        if self.cookie_secret.len() < 24 {
            return Err("AKURAI_ROUTER_COOKIE_SECRET must be at least 24 characters".to_string());
        }
        if self.idp_client_id.is_empty() || self.idp_client_secret.is_empty() {
            return Err(
                "AKURAI_ROUTER_IDP_CLIENT_ID and AKURAI_ROUTER_IDP_CLIENT_SECRET are required for admin login"
                    .to_string(),
            );
        }
        Ok(())
    }
}

pub fn ensure_default_files(cfg: &Config) -> Result<(), String> {
    let providers_path = cfg.data_dir.join("providers.conf");
    let mut providers = load_providers(cfg);
    let mut providers_changed = false;
    if !providers.iter().any(|p| p.id == "codex") {
        providers.push(Provider {
            id: "codex".to_string(),
            name: "OpenAI Codex".to_string(),
            enabled: true,
            auth_path: cfg.codex_auth_path.clone(),
        });
        providers_changed = true;
    }
    if !providers.iter().any(|p| p.id == "claude") {
        providers.push(Provider {
            id: "claude".to_string(),
            name: "Claude Code".to_string(),
            enabled: true,
            auth_path: cfg.claude_auth_path.clone(),
        });
        providers_changed = true;
    }
    if !providers_path.exists() || providers_changed {
        save_providers(cfg, &providers)?;
    }
    let models_path = cfg.data_dir.join("models.conf");
    let mut models = load_models(cfg);
    let mut models_changed = false;
    for default_model in default_models() {
        if !models.iter().any(|m| m.id == default_model.id) {
            models.push(default_model);
            models_changed = true;
        }
    }
    if !models_path.exists() || models_changed {
        save_models(cfg, &models)?;
    }
    Ok(())
}

pub fn default_models() -> Vec<Model> {
    [
        ("gpt-5.5", "GPT 5.5", "gpt-5.5", "codex"),
        ("gpt-5.4", "GPT 5.4", "gpt-5.4", "codex"),
        ("gpt-5.4-mini", "GPT 5.4 Mini", "gpt-5.4-mini", "codex"),
        ("gpt-5.3-codex", "GPT 5.3 Codex", "gpt-5.3-codex", "codex"),
        (
            "gpt-5.3-codex-high",
            "GPT 5.3 Codex High",
            "gpt-5.3-codex",
            "codex",
        ),
        (
            "gpt-5.3-codex-low",
            "GPT 5.3 Codex Low",
            "gpt-5.3-codex",
            "codex",
        ),
        (
            "claude-opus-4-7",
            "Claude Opus 4.7",
            "claude-opus-4-7",
            "claude",
        ),
        (
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6",
            "claude-sonnet-4-6",
            "claude",
        ),
        (
            "claude-haiku-4-5-20251001",
            "Claude Haiku 4.5",
            "claude-haiku-4-5-20251001",
            "claude",
        ),
    ]
    .into_iter()
    .map(|(id, name, upstream_id, provider_id)| Model {
        id: id.to_string(),
        name: name.to_string(),
        upstream_id: upstream_id.to_string(),
        provider_id: provider_id.to_string(),
        enabled: true,
    })
    .collect()
}

pub fn load_models(cfg: &Config) -> Vec<Model> {
    let path = cfg.data_dir.join("models.conf");
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut models = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 4 {
            continue;
        }
        models.push(Model {
            id: parts[0].to_string(),
            name: parts[1].to_string(),
            upstream_id: parts[2].to_string(),
            provider_id: if parts.len() >= 5 {
                parts[3].to_string()
            } else {
                infer_model_provider_id(parts[0])
            },
            enabled: if parts.len() >= 5 {
                parts[4] != "false"
            } else {
                parts[3] != "false"
            },
        });
    }
    if models.is_empty() {
        default_models()
    } else {
        models
    }
}

pub fn save_models(cfg: &Config, models: &[Model]) -> Result<(), String> {
    let mut out = String::from("# id|name|upstream_id|provider_id|enabled\n");
    for model in models {
        out.push_str(&format!(
            "{}|{}|{}|{}|{}\n",
            model.id,
            model.name,
            model.upstream_id,
            model.provider_id,
            if model.enabled { "true" } else { "false" }
        ));
    }
    fs::write(cfg.data_dir.join("models.conf"), out).map_err(|e| e.to_string())
}

pub fn load_providers(cfg: &Config) -> Vec<Provider> {
    let text = fs::read_to_string(cfg.data_dir.join("providers.conf")).unwrap_or_default();
    let mut providers = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 4 {
            continue;
        }
        providers.push(Provider {
            id: parts[0].to_string(),
            name: parts[1].to_string(),
            enabled: parts[2] != "false",
            auth_path: PathBuf::from(parts[3]),
        });
    }
    if providers.is_empty() {
        vec![
            Provider {
                id: "codex".to_string(),
                name: "OpenAI Codex".to_string(),
                enabled: true,
                auth_path: cfg.codex_auth_path.clone(),
            },
            Provider {
                id: "claude".to_string(),
                name: "Claude Code".to_string(),
                enabled: true,
                auth_path: cfg.claude_auth_path.clone(),
            },
        ]
    } else {
        providers
    }
}

pub fn save_providers(cfg: &Config, providers: &[Provider]) -> Result<(), String> {
    let mut out = String::from("# id|name|enabled|auth_path\n");
    for provider in providers {
        out.push_str(&format!(
            "{}|{}|{}|{}\n",
            provider.id,
            provider.name,
            if provider.enabled { "true" } else { "false" },
            provider.auth_path.display()
        ));
    }
    fs::write(cfg.data_dir.join("providers.conf"), out).map_err(|e| e.to_string())
}

pub fn write_local_env_template(cfg: &Config) -> Result<PathBuf, String> {
    fs::create_dir_all(&cfg.data_dir).map_err(|e| e.to_string())?;
    let path = cfg.data_dir.join("router.conf");
    let api_key = if cfg.api_key.is_empty() {
        format!("akr_{}", random_hex(32))
    } else {
        cfg.api_key.clone()
    };
    let cookie_secret = if cfg.cookie_secret.is_empty() {
        random_hex(48)
    } else {
        cfg.cookie_secret.clone()
    };
    let content = format!(
        "AKURAI_ROUTER_LISTEN={}\nAKURAI_ROUTER_PUBLIC_URL={}\nAKURAI_ROUTER_API_KEY={}\nAKURAI_ROUTER_COOKIE_SECRET={}\nAKURAI_ROUTER_CODEX_AUTH_PATH={}\nAKURAI_ROUTER_CLAUDE_AUTH_PATH={}\nAKURAI_ROUTER_DEFAULT_MODEL={}\nAKURAI_ROUTER_IDP_ISSUER={}\nAKURAI_ROUTER_IDP_CLIENT_ID={}\nAKURAI_ROUTER_IDP_CLIENT_SECRET={}\nAKURAI_ROUTER_ADMIN_EMAIL={}\n",
        env_quote(&cfg.listen_addr),
        env_quote(&cfg.public_base_url),
        env_quote(&api_key),
        env_quote(&cookie_secret),
        env_quote(&cfg.codex_auth_path.display().to_string()),
        env_quote(&cfg.claude_auth_path.display().to_string()),
        env_quote(&cfg.default_model),
        env_quote(&cfg.idp_issuer),
        env_quote(&cfg.idp_client_id),
        env_quote(&cfg.idp_client_secret),
        env_quote(&cfg.admin_allowed_email),
    );
    fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(path)
}

fn infer_model_provider_id(model_id: &str) -> String {
    if model_id.starts_with("claude-") || model_id.starts_with("cc/claude-") {
        "claude".to_string()
    } else {
        "codex".to_string()
    }
}

fn read_kv(path: &PathBuf) -> Vec<(String, String)> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let mut parts = trimmed.splitn(2, '=');
            let key = parts.next()?.trim().to_string();
            let mut value = parts.next().unwrap_or("").trim().to_string();
            if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
                value = value[1..value.len() - 1]
                    .replace("\\\"", "\"")
                    .replace("\\\\", "\\");
            }
            Some((key, value))
        })
        .collect()
}
