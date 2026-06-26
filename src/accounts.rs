use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use crate::config::Config;
use crate::util::{now_secs, random_hex};

#[derive(Clone, Debug)]
pub struct RouterUser {
    pub email: String,
    pub name: String,
    pub enabled: bool,
    pub cost_share_pct: f64,
}

#[derive(Clone, Debug)]
pub struct ClientKey {
    pub id: String,
    pub email: String,
    pub name: String,
    pub key: String,
    pub enabled: bool,
    pub created_at: u64,
    pub last_used_at: u64,
}

#[derive(Clone, Debug, Default)]
pub struct BillingConfig {
    pub monthly_shared_cost_usd: f64,
}

#[derive(Clone, Debug, Default)]
pub struct UsageRecord {
    pub ts: u64,
    pub email: String,
    pub key_id: String,
    pub provider_id: String,
    pub model: String,
    pub endpoint: String,
    pub status: u16,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Clone, Debug, Default)]
pub struct UsageSummary {
    pub email: String,
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
}

pub fn ensure_account_files(cfg: &Config) -> Result<(), String> {
    fs::create_dir_all(&cfg.data_dir).map_err(|e| e.to_string())?;
    let users_path = cfg.data_dir.join("users.conf");
    if !users_path.exists() {
        let users = vec![RouterUser {
            email: cfg.admin_allowed_email.clone(),
            name: "Ólafur Búi Ólafsson".to_string(),
            enabled: true,
            cost_share_pct: 100.0,
        }];
        save_users(cfg, &users)?;
    }
    let keys_path = cfg.data_dir.join("client_keys.conf");
    if !keys_path.exists() {
        fs::write(
            &keys_path,
            "# id|email|name|key|enabled|created_at|last_used_at\n",
        )
        .map_err(|e| e.to_string())?;
        set_private_permissions(&keys_path);
    }
    let usage_path = cfg.data_dir.join("usage.tsv");
    if !usage_path.exists() {
        fs::write(
            &usage_path,
            "# ts\temail\tkey_id\tprovider_id\tmodel\tendpoint\tstatus\tprompt_tokens\tcompletion_tokens\ttotal_tokens\tcost_usd\n",
        )
        .map_err(|e| e.to_string())?;
        set_private_permissions(&usage_path);
    }
    let billing_path = cfg.data_dir.join("billing.conf");
    if !billing_path.exists() {
        save_billing_config(cfg, &BillingConfig::default())?;
    }
    Ok(())
}

pub fn load_users(cfg: &Config) -> Vec<RouterUser> {
    let text = fs::read_to_string(cfg.data_dir.join("users.conf")).unwrap_or_default();
    let mut users = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 4 {
            continue;
        }
        users.push(RouterUser {
            email: parts[0].to_string(),
            name: parts[1].to_string(),
            enabled: parts[2] != "false",
            cost_share_pct: parse_f64(parts[3]).unwrap_or(0.0),
        });
    }
    if users.is_empty() {
        vec![RouterUser {
            email: cfg.admin_allowed_email.clone(),
            name: "Ólafur Búi Ólafsson".to_string(),
            enabled: true,
            cost_share_pct: 100.0,
        }]
    } else {
        users
    }
}

pub fn save_users(cfg: &Config, users: &[RouterUser]) -> Result<(), String> {
    let mut out = String::from("# email|name|enabled|cost_share_pct\n");
    for user in users {
        if user.email.trim().is_empty() {
            continue;
        }
        out.push_str(&format!(
            "{}|{}|{}|{:.4}\n",
            clean_field(&user.email.to_ascii_lowercase()),
            clean_field(&user.name),
            if user.enabled { "true" } else { "false" },
            user.cost_share_pct.clamp(0.0, 100.0)
        ));
    }
    let path = cfg.data_dir.join("users.conf");
    fs::write(&path, out).map_err(|e| e.to_string())?;
    set_private_permissions(&path);
    Ok(())
}

pub fn upsert_user(cfg: &Config, mut user: RouterUser) -> Result<(), String> {
    user.email = user.email.trim().to_ascii_lowercase();
    if user.email.is_empty() {
        return Err("user email is required".to_string());
    }
    let mut users = load_users(cfg);
    users.retain(|existing| !existing.email.eq_ignore_ascii_case(&user.email));
    users.push(user);
    save_users(cfg, &users)
}

pub fn user_enabled(cfg: &Config, email: &str) -> bool {
    load_users(cfg)
        .into_iter()
        .find(|u| u.email.eq_ignore_ascii_case(email))
        .map(|u| u.enabled)
        .unwrap_or(false)
}

pub fn load_client_keys(cfg: &Config) -> Vec<ClientKey> {
    let text = fs::read_to_string(cfg.data_dir.join("client_keys.conf")).unwrap_or_default();
    let mut keys = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 7 {
            continue;
        }
        keys.push(ClientKey {
            id: parts[0].to_string(),
            email: parts[1].to_string(),
            name: parts[2].to_string(),
            key: parts[3].to_string(),
            enabled: parts[4] != "false",
            created_at: parts[5].parse().unwrap_or(0),
            last_used_at: parts[6].parse().unwrap_or(0),
        });
    }
    keys
}

pub fn save_client_keys(cfg: &Config, keys: &[ClientKey]) -> Result<(), String> {
    let mut out = String::from("# id|email|name|key|enabled|created_at|last_used_at\n");
    for key in keys {
        if key.id.trim().is_empty() || key.email.trim().is_empty() || key.key.trim().is_empty() {
            continue;
        }
        out.push_str(&format!(
            "{}|{}|{}|{}|{}|{}|{}\n",
            clean_field(&key.id),
            clean_field(&key.email.to_ascii_lowercase()),
            clean_field(&key.name),
            clean_field(&key.key),
            if key.enabled { "true" } else { "false" },
            key.created_at,
            key.last_used_at
        ));
    }
    let path = cfg.data_dir.join("client_keys.conf");
    fs::write(&path, out).map_err(|e| e.to_string())?;
    set_private_permissions(&path);
    Ok(())
}

pub fn create_client_key(
    cfg: &Config,
    email: &str,
    name: &str,
) -> Result<(ClientKey, String), String> {
    let email = email.trim().to_ascii_lowercase();
    if email.is_empty() {
        return Err("key owner email is required".to_string());
    }
    if !user_enabled(cfg, &email) {
        return Err("key owner must exist and be enabled".to_string());
    }
    let plaintext = format!("akr_user_{}", random_hex(32));
    let key = ClientKey {
        id: format!("key_{}", random_hex(8)),
        email,
        name: if name.trim().is_empty() {
            "Router API key".to_string()
        } else {
            name.trim().to_string()
        },
        key: plaintext.clone(),
        enabled: true,
        created_at: now_secs(),
        last_used_at: 0,
    };
    let mut keys = load_client_keys(cfg);
    keys.push(key.clone());
    save_client_keys(cfg, &keys)?;
    Ok((key, plaintext))
}

pub fn set_client_key_enabled(cfg: &Config, key_id: &str, enabled: bool) -> Result<(), String> {
    let mut keys = load_client_keys(cfg);
    for key in &mut keys {
        if key.id == key_id {
            key.enabled = enabled;
        }
    }
    save_client_keys(cfg, &keys)
}

pub fn find_client_key(cfg: &Config, token: &str) -> Option<ClientKey> {
    load_client_keys(cfg)
        .into_iter()
        .find(|key| key.enabled && constant_time_eq(key.key.as_bytes(), token.as_bytes()))
        .filter(|key| user_enabled(cfg, &key.email))
}

pub fn touch_client_key(cfg: &Config, key_id: &str) {
    let mut keys = load_client_keys(cfg);
    let now = now_secs();
    let mut changed = false;
    for key in &mut keys {
        if key.id == key_id {
            key.last_used_at = now;
            changed = true;
        }
    }
    if changed {
        let _ = save_client_keys(cfg, &keys);
    }
}

pub fn load_billing_config(cfg: &Config) -> BillingConfig {
    let text = fs::read_to_string(cfg.data_dir.join("billing.conf")).unwrap_or_default();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("monthly_shared_cost_usd=") {
            return BillingConfig {
                monthly_shared_cost_usd: parse_f64(
                    trimmed
                        .split_once('=')
                        .map(|(_, value)| value)
                        .unwrap_or("0"),
                )
                .unwrap_or(0.0),
            };
        }
    }
    BillingConfig::default()
}

pub fn save_billing_config(cfg: &Config, billing: &BillingConfig) -> Result<(), String> {
    let path = cfg.data_dir.join("billing.conf");
    fs::write(
        &path,
        format!(
            "monthly_shared_cost_usd={:.4}\n",
            billing.monthly_shared_cost_usd.max(0.0)
        ),
    )
    .map_err(|e| e.to_string())?;
    set_private_permissions(&path);
    Ok(())
}

pub fn record_usage(cfg: &Config, record: &UsageRecord) -> Result<(), String> {
    fs::create_dir_all(&cfg.data_dir).map_err(|e| e.to_string())?;
    let path = cfg.data_dir.join("usage.tsv");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    writeln!(
        file,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.8}",
        record.ts,
        clean_tsv(&record.email),
        clean_tsv(&record.key_id),
        clean_tsv(&record.provider_id),
        clean_tsv(&record.model),
        clean_tsv(&record.endpoint),
        record.status,
        record.prompt_tokens,
        record.completion_tokens,
        record.total_tokens,
        record.cost_usd.max(0.0)
    )
    .map_err(|e| e.to_string())?;
    set_private_permissions(&path);
    Ok(())
}

pub fn load_usage_records(cfg: &Config) -> Vec<UsageRecord> {
    let text = fs::read_to_string(cfg.data_dir.join("usage.tsv")).unwrap_or_default();
    let mut records = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 11 {
            continue;
        }
        records.push(UsageRecord {
            ts: parts[0].parse().unwrap_or(0),
            email: parts[1].to_string(),
            key_id: parts[2].to_string(),
            provider_id: parts[3].to_string(),
            model: parts[4].to_string(),
            endpoint: parts[5].to_string(),
            status: parts[6].parse().unwrap_or(0),
            prompt_tokens: parts[7].parse().unwrap_or(0),
            completion_tokens: parts[8].parse().unwrap_or(0),
            total_tokens: parts[9].parse().unwrap_or(0),
            cost_usd: parse_f64(parts[10]).unwrap_or(0.0),
        });
    }
    records
}

pub fn usage_summaries(cfg: &Config) -> Vec<UsageSummary> {
    let mut summaries: Vec<UsageSummary> = Vec::new();
    for record in load_usage_records(cfg) {
        let email = if record.email.trim().is_empty() {
            "unassigned".to_string()
        } else {
            record.email
        };
        let idx = summaries.iter().position(|s| s.email == email);
        let summary = match idx {
            Some(idx) => &mut summaries[idx],
            None => {
                summaries.push(UsageSummary {
                    email: email.clone(),
                    ..UsageSummary::default()
                });
                summaries.last_mut().expect("summary was just pushed")
            }
        };
        summary.requests += 1;
        summary.prompt_tokens += record.prompt_tokens;
        summary.completion_tokens += record.completion_tokens;
        summary.total_tokens += record.total_tokens;
        summary.cost_usd += record.cost_usd;
    }
    summaries.sort_by(|a, b| a.email.cmp(&b.email));
    summaries
}

pub fn key_hint(key: &str) -> String {
    if key.len() <= 18 {
        return "stored".to_string();
    }
    format!("{}...{}", &key[..12], &key[key.len() - 6..])
}

fn clean_field(value: &str) -> String {
    value.replace(['|', '\n', '\r'], " ").trim().to_string()
}

fn clean_tsv(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ").trim().to_string()
}

fn parse_f64(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn set_private_permissions(path: &std::path::Path) {
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use crate::http::Request;
    use std::path::PathBuf;

    fn test_config(name: &str) -> Config {
        let data_dir = std::env::temp_dir().join(format!(
            "akurai-router-accounts-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&data_dir);
        std::fs::create_dir_all(&data_dir).unwrap();
        Config {
            listen_addr: "127.0.0.1:0".to_string(),
            public_base_url: "http://127.0.0.1:0".to_string(),
            data_dir,
            api_key: "akr_global".to_string(),
            codex_auth_path: PathBuf::from("/tmp/codex-auth.json"),
            codex_responses_url: "https://example.com/codex".to_string(),
            codex_models_url: "https://example.com/codex-models".to_string(),
            claude_auth_path: PathBuf::from("/tmp/claude-auth.json"),
            claude_messages_url: "https://example.com/claude".to_string(),
            claude_models_url: "https://example.com/claude-models".to_string(),
            opencode_go_auth_path: PathBuf::from("/tmp/opencode-auth.json"),
            opencode_go_chat_url: "https://example.com/opencode-chat".to_string(),
            opencode_go_messages_url: "https://example.com/opencode-messages".to_string(),
            opencode_go_models_url: "https://example.com/opencode-models".to_string(),
            default_model: "gpt-5.4-mini".to_string(),
            idp_issuer: "https://auth.example.com".to_string(),
            idp_client_id: "client".to_string(),
            idp_client_secret: "secret".to_string(),
            admin_allowed_email: "olibuijr@olibuijr.com".to_string(),
            cookie_secret: "012345678901234567890123456789".to_string(),
        }
    }

    fn bearer_request(token: &str) -> Request {
        Request {
            method: "GET".to_string(),
            path: "/v1/models".to_string(),
            query: String::new(),
            headers: vec![("Authorization".to_string(), format!("Bearer {token}"))],
            body: Vec::new(),
        }
    }

    #[test]
    fn generated_client_key_authenticates_as_assigned_user() {
        let cfg = test_config("client-key-auth");
        ensure_account_files(&cfg).unwrap();
        upsert_user(
            &cfg,
            RouterUser {
                email: "dev@example.com".to_string(),
                name: "Dev User".to_string(),
                enabled: true,
                cost_share_pct: 25.0,
            },
        )
        .unwrap();
        let (key, plaintext) = create_client_key(&cfg, "dev@example.com", "dev laptop").unwrap();
        let actor = auth::authenticate_api_key(&bearer_request(&plaintext), &cfg).unwrap();
        assert_eq!(actor.email, "dev@example.com");
        assert_eq!(actor.key_id, key.id);
    }

    #[test]
    fn usage_summaries_group_by_user() {
        let cfg = test_config("usage-summary");
        ensure_account_files(&cfg).unwrap();
        record_usage(
            &cfg,
            &UsageRecord {
                ts: 1,
                email: "dev@example.com".to_string(),
                key_id: "key_1".to_string(),
                provider_id: "opencode-go".to_string(),
                model: "opencode-go/glm-5.2".to_string(),
                endpoint: "/v1/chat/completions".to_string(),
                status: 200,
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cost_usd: 0.0123,
            },
        )
        .unwrap();
        let summaries = usage_summaries(&cfg);
        let summary = summaries
            .iter()
            .find(|item| item.email == "dev@example.com")
            .unwrap();
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.total_tokens, 15);
        assert!((summary.cost_usd - 0.0123).abs() < 0.00001);
    }
}
