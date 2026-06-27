use std::env;
use std::fs;
use std::path::PathBuf;

use crate::util::{env_quote, random_hex};

pub const PROVIDER_CODEX: &str = "codex";
pub const PROVIDER_CLAUDE: &str = "claude";
pub const PROVIDER_OPENCODE_GO: &str = "opencode-go";
pub const PROVIDER_EMBEDDINGS: &str = "embeddings";
pub const DEFAULT_EMBEDDING_MODEL: &str = "intfloat/multilingual-e5-small";
pub const DEFAULT_EMBEDDING_UPSTREAM_URL: &str = "http://127.0.0.1:8081/v1/embeddings";

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
    pub opencode_go_auth_path: PathBuf,
    pub opencode_go_chat_url: String,
    pub opencode_go_messages_url: String,
    pub opencode_go_models_url: String,
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

#[derive(Clone, Debug)]
pub struct EmbeddingConfig {
    pub enabled: bool,
    pub upstream_url: String,
    pub model: String,
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
            opencode_go_auth_path: PathBuf::from(get(
                "AKURAI_ROUTER_OPENCODE_GO_AUTH_PATH",
                &format!("{home}/.local/share/opencode/auth.json"),
            )),
            opencode_go_chat_url: get(
                "AKURAI_ROUTER_OPENCODE_GO_CHAT_URL",
                "https://opencode.ai/zen/go/v1/chat/completions",
            ),
            opencode_go_messages_url: get(
                "AKURAI_ROUTER_OPENCODE_GO_MESSAGES_URL",
                "https://opencode.ai/zen/go/v1/messages",
            ),
            opencode_go_models_url: get(
                "AKURAI_ROUTER_OPENCODE_GO_MODELS_URL",
                "https://opencode.ai/zen/go/v1/models",
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
    for provider_id in [PROVIDER_CODEX, PROVIDER_CLAUDE, PROVIDER_OPENCODE_GO] {
        if !providers.iter().any(|p| p.id == provider_id) {
            providers.push(Provider {
                id: provider_id.to_string(),
                name: provider_display_name(provider_id).to_string(),
                enabled: true,
                auth_path: default_provider_auth_path(cfg, provider_id),
            });
            providers_changed = true;
        }
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
    let embeddings_path = cfg.data_dir.join("embeddings.conf");
    if !embeddings_path.exists() {
        save_embedding_config(cfg, &load_embedding_config(cfg))?;
    }
    Ok(())
}

pub fn default_models() -> Vec<Model> {
    let mut models = [
        ("gpt-5.5", "GPT 5.5", "gpt-5.5", PROVIDER_CODEX),
        ("gpt-5.4", "GPT 5.4", "gpt-5.4", PROVIDER_CODEX),
        (
            "gpt-5.4-mini",
            "GPT 5.4 Mini",
            "gpt-5.4-mini",
            PROVIDER_CODEX,
        ),
        (
            "gpt-5.3-codex",
            "GPT 5.3 Codex",
            "gpt-5.3-codex",
            PROVIDER_CODEX,
        ),
        (
            "gpt-5.3-codex-high",
            "GPT 5.3 Codex High",
            "gpt-5.3-codex",
            PROVIDER_CODEX,
        ),
        (
            "gpt-5.3-codex-low",
            "GPT 5.3 Codex Low",
            "gpt-5.3-codex",
            PROVIDER_CODEX,
        ),
        (
            "claude-opus-4-8",
            "Claude Opus 4.8",
            "claude-opus-4-8",
            PROVIDER_CLAUDE,
        ),
        (
            "claude-opus-4-7",
            "Claude Opus 4.7",
            "claude-opus-4-7",
            PROVIDER_CLAUDE,
        ),
        (
            "claude-opus-4-6",
            "Claude Opus 4.6",
            "claude-opus-4-6",
            PROVIDER_CLAUDE,
        ),
        (
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6",
            "claude-sonnet-4-6",
            PROVIDER_CLAUDE,
        ),
        (
            "claude-opus-4-5-20251101",
            "Claude Opus 4.5",
            "claude-opus-4-5-20251101",
            PROVIDER_CLAUDE,
        ),
        (
            "claude-haiku-4-5-20251001",
            "Claude Haiku 4.5",
            "claude-haiku-4-5-20251001",
            PROVIDER_CLAUDE,
        ),
        (
            "claude-sonnet-4-5-20250929",
            "Claude Sonnet 4.5",
            "claude-sonnet-4-5-20250929",
            PROVIDER_CLAUDE,
        ),
        (
            "claude-opus-4-1-20250805",
            "Claude Opus 4.1",
            "claude-opus-4-1-20250805",
            PROVIDER_CLAUDE,
        ),
        (
            "multilingual-e5-small",
            "Multilingual E5 Small",
            DEFAULT_EMBEDDING_MODEL,
            PROVIDER_EMBEDDINGS,
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
    .collect::<Vec<_>>();
    models.extend(opencode_go_default_models());
    models
}

pub fn opencode_go_default_models() -> Vec<Model> {
    [
        ("glm-5.2", "GLM 5.2", "glm-5.2"),
        ("glm-5.1", "GLM 5.1", "glm-5.1"),
        ("kimi-k2.7-code", "Kimi K2.7 Code", "kimi-k2.7-code"),
        ("kimi-k2.6", "Kimi K2.6", "kimi-k2.6"),
        ("deepseek-v4-pro", "DeepSeek V4 Pro", "deepseek-v4-pro"),
        (
            "deepseek-v4-flash",
            "DeepSeek V4 Flash",
            "deepseek-v4-flash",
        ),
        ("mimo-v2.5", "MiMo V2.5", "mimo-v2.5"),
        ("mimo-v2.5-pro", "MiMo V2.5 Pro", "mimo-v2.5-pro"),
        ("minimax-m3", "MiniMax M3", "minimax-m3"),
        ("minimax-m2.7", "MiniMax M2.7", "minimax-m2.7"),
        ("minimax-m2.5", "MiniMax M2.5", "minimax-m2.5"),
        ("qwen3.7-max", "Qwen 3.7 Max", "qwen3.7-max"),
        ("qwen3.7-plus", "Qwen 3.7 Plus", "qwen3.7-plus"),
        ("qwen3.6-plus", "Qwen 3.6 Plus", "qwen3.6-plus"),
    ]
    .into_iter()
    .map(|(id, name, upstream_id)| Model {
        id: id.to_string(),
        name: name.to_string(),
        upstream_id: upstream_id.to_string(),
        provider_id: PROVIDER_OPENCODE_GO.to_string(),
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
        [PROVIDER_CODEX, PROVIDER_CLAUDE, PROVIDER_OPENCODE_GO]
            .into_iter()
            .map(|provider_id| Provider {
                id: provider_id.to_string(),
                name: provider_display_name(provider_id).to_string(),
                enabled: true,
                auth_path: default_provider_auth_path(cfg, provider_id),
            })
            .collect()
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

pub fn load_embedding_config(cfg: &Config) -> EmbeddingConfig {
    let conf = read_kv(&cfg.data_dir.join("embeddings.conf"));
    let get = |key: &str, default: &str| -> String {
        conf.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .or_else(|| env::var(key).ok())
            .unwrap_or_else(|| default.to_string())
    };
    EmbeddingConfig {
        enabled: config_bool(&get("AKURAI_ROUTER_EMBEDDINGS_ENABLED", "true"), true),
        upstream_url: get(
            "AKURAI_ROUTER_EMBEDDINGS_URL",
            DEFAULT_EMBEDDING_UPSTREAM_URL,
        ),
        model: get("AKURAI_ROUTER_EMBEDDINGS_MODEL", DEFAULT_EMBEDDING_MODEL),
    }
}

pub fn save_embedding_config(cfg: &Config, embedding: &EmbeddingConfig) -> Result<(), String> {
    let content = format!(
        "AKURAI_ROUTER_EMBEDDINGS_ENABLED={}\nAKURAI_ROUTER_EMBEDDINGS_URL={}\nAKURAI_ROUTER_EMBEDDINGS_MODEL={}\n",
        if embedding.enabled { "true" } else { "false" },
        env_quote(&embedding.upstream_url),
        env_quote(&embedding.model),
    );
    fs::write(cfg.data_dir.join("embeddings.conf"), content).map_err(|e| e.to_string())
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
        "AKURAI_ROUTER_LISTEN={}\nAKURAI_ROUTER_PUBLIC_URL={}\nAKURAI_ROUTER_API_KEY={}\nAKURAI_ROUTER_COOKIE_SECRET={}\nAKURAI_ROUTER_CODEX_AUTH_PATH={}\nAKURAI_ROUTER_CLAUDE_AUTH_PATH={}\nAKURAI_ROUTER_OPENCODE_GO_AUTH_PATH={}\nAKURAI_ROUTER_DEFAULT_MODEL={}\nAKURAI_ROUTER_EMBEDDINGS_URL={}\nAKURAI_ROUTER_EMBEDDINGS_MODEL={}\nAKURAI_ROUTER_EMBEDDINGS_ENABLED=true\nAKURAI_ROUTER_IDP_ISSUER={}\nAKURAI_ROUTER_IDP_CLIENT_ID={}\nAKURAI_ROUTER_IDP_CLIENT_SECRET={}\nAKURAI_ROUTER_ADMIN_EMAIL={}\n",
        env_quote(&cfg.listen_addr),
        env_quote(&cfg.public_base_url),
        env_quote(&api_key),
        env_quote(&cookie_secret),
        env_quote(&cfg.codex_auth_path.display().to_string()),
        env_quote(&cfg.claude_auth_path.display().to_string()),
        env_quote(&cfg.opencode_go_auth_path.display().to_string()),
        env_quote(&cfg.default_model),
        env_quote(DEFAULT_EMBEDDING_UPSTREAM_URL),
        env_quote(DEFAULT_EMBEDDING_MODEL),
        env_quote(&cfg.idp_issuer),
        env_quote(&cfg.idp_client_id),
        env_quote(&cfg.idp_client_secret),
        env_quote(&cfg.admin_allowed_email),
    );
    fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(path)
}

pub fn infer_model_provider_id(model_id: &str) -> String {
    if model_id == DEFAULT_EMBEDDING_MODEL
        || model_id.starts_with("multilingual-e5")
        || model_id.starts_with("intfloat/")
    {
        return PROVIDER_EMBEDDINGS.to_string();
    }
    if let Some((provider_id, _)) = split_model_provider_prefix(model_id) {
        return provider_id;
    } else if model_id.starts_with("claude-") {
        PROVIDER_CLAUDE.to_string()
    } else if is_opencode_go_model(model_id) {
        PROVIDER_OPENCODE_GO.to_string()
    } else {
        PROVIDER_CODEX.to_string()
    }
}

pub fn default_provider_auth_path(cfg: &Config, provider_id: &str) -> PathBuf {
    match canonical_provider_id(provider_id).as_str() {
        PROVIDER_CLAUDE => cfg.claude_auth_path.clone(),
        PROVIDER_OPENCODE_GO => cfg.opencode_go_auth_path.clone(),
        _ => cfg.codex_auth_path.clone(),
    }
}

pub fn provider_display_name(provider_id: &str) -> &'static str {
    match canonical_provider_id(provider_id).as_str() {
        PROVIDER_CLAUDE => "Claude Code",
        PROVIDER_OPENCODE_GO => "OpenCode Go",
        PROVIDER_EMBEDDINGS => "Embeddings",
        _ => "OpenAI Codex",
    }
}

pub fn canonical_provider_id(provider_id: &str) -> String {
    match provider_id {
        "cx" => PROVIDER_CODEX.to_string(),
        "cc" => PROVIDER_CLAUDE.to_string(),
        "opencode" | "ocg" => PROVIDER_OPENCODE_GO.to_string(),
        "embed" | "embedding" => PROVIDER_EMBEDDINGS.to_string(),
        other => other.to_string(),
    }
}

pub fn split_model_provider_prefix(model_id: &str) -> Option<(String, String)> {
    let (provider, model) = model_id.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some((canonical_provider_id(provider), model.to_string()))
}

pub fn model_id_for_provider(model_id: &str, provider_id: &str) -> String {
    if let Some((prefix_provider, bare_model)) = split_model_provider_prefix(model_id) {
        if prefix_provider == canonical_provider_id(provider_id) {
            return bare_model;
        }
    }
    model_id.to_string()
}

pub fn public_model_id(model: &Model) -> String {
    if split_model_provider_prefix(&model.id).is_some() {
        model.id.clone()
    } else {
        format!("{}/{}", canonical_provider_id(&model.provider_id), model.id)
    }
}

pub fn is_opencode_go_messages_model(model_id: &str) -> bool {
    let bare = model_id_for_provider(model_id, PROVIDER_OPENCODE_GO);
    matches!(
        bare.as_str(),
        "minimax-m3"
            | "minimax-m2.7"
            | "minimax-m2.5"
            | "qwen3.7-max"
            | "qwen3.7-plus"
            | "qwen3.6-plus"
    )
}

pub fn is_opencode_go_model(model_id: &str) -> bool {
    let bare = model_id_for_provider(model_id, PROVIDER_OPENCODE_GO);
    opencode_go_default_models()
        .into_iter()
        .any(|model| model.id == bare)
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

fn config_bool(value: &str, default: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => true,
        "0" | "false" | "no" | "off" | "disabled" => false,
        _ => default,
    }
}
