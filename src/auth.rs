use std::fs::{self, OpenOptions};
use std::io::Write;

use crate::config::Config;
use crate::http::Request;
use crate::util::{now_secs, random_hex};

const SESSION_COOKIE: &str = "akurai_router_session";
const STATE_COOKIE: &str = "akurai_router_state";

#[derive(Clone, Debug)]
pub struct AdminSession {
    pub token: String,
    pub email: String,
    pub expires_at: u64,
}

pub fn check_api_key(req: &Request, cfg: &Config) -> bool {
    let Some(auth) = req.header("authorization") else {
        return false;
    };
    let token = auth.strip_prefix("Bearer ").unwrap_or(auth).trim();
    constant_time_eq(token.as_bytes(), cfg.api_key.as_bytes())
}

pub fn create_oauth_state(cfg: &Config) -> Result<String, String> {
    let state = random_hex(24);
    let expires = now_secs() + 600;
    fs::create_dir_all(cfg.data_dir.join("oauth")).map_err(|e| e.to_string())?;
    fs::write(cfg.data_dir.join("oauth").join(&state), expires.to_string())
        .map_err(|e| e.to_string())?;
    Ok(state)
}

pub fn validate_oauth_state(req: &Request, cfg: &Config, state: &str) -> bool {
    if state.len() < 16 || req.cookie(STATE_COOKIE).as_deref() != Some(state) {
        return false;
    }
    let path = cfg.data_dir.join("oauth").join(state);
    let Ok(text) = fs::read_to_string(&path) else {
        return false;
    };
    let _ = fs::remove_file(path);
    let expires = text.trim().parse::<u64>().unwrap_or(0);
    expires >= now_secs()
}

pub fn state_cookie(cfg: &Config, state: &str) -> (&'static str, String) {
    (
        "Set-Cookie",
        format!(
            "{STATE_COOKIE}={state}; Path=/; Max-Age=600; HttpOnly; SameSite=Lax{}",
            secure_cookie_suffix(cfg)
        ),
    )
}

pub fn clear_state_cookie(cfg: &Config) -> (&'static str, String) {
    (
        "Set-Cookie",
        format!(
            "{STATE_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{}",
            secure_cookie_suffix(cfg)
        ),
    )
}

pub fn create_session(cfg: &Config, email: &str) -> Result<String, String> {
    let token = random_hex(32);
    let expires_at = now_secs() + 12 * 3600;
    fs::create_dir_all(&cfg.data_dir).map_err(|e| e.to_string())?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.data_dir.join("sessions.tsv"))
        .map_err(|e| e.to_string())?;
    writeln!(file, "{token}\t{email}\t{expires_at}").map_err(|e| e.to_string())?;
    Ok(token)
}

pub fn session_cookie(cfg: &Config, token: &str) -> (&'static str, String) {
    (
        "Set-Cookie",
        format!(
            "{SESSION_COOKIE}={token}; Path=/; Max-Age=43200; HttpOnly; SameSite=Lax{}",
            secure_cookie_suffix(cfg)
        ),
    )
}

pub fn clear_session_cookie(cfg: &Config) -> (&'static str, String) {
    (
        "Set-Cookie",
        format!(
            "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{}",
            secure_cookie_suffix(cfg)
        ),
    )
}

pub fn admin_session(req: &Request, cfg: &Config) -> Option<AdminSession> {
    let token = req.cookie(SESSION_COOKIE)?;
    let text = fs::read_to_string(cfg.data_dir.join("sessions.tsv")).ok()?;
    let now = now_secs();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 3 {
            continue;
        }
        if parts[0] == token {
            let expires_at = parts[2].parse::<u64>().ok()?;
            if expires_at < now {
                return None;
            }
            return Some(AdminSession {
                token,
                email: parts[1].to_string(),
                expires_at,
            });
        }
    }
    None
}

pub fn remove_session(cfg: &Config, token: &str) {
    let path = cfg.data_dir.join("sessions.tsv");
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let next = text
        .lines()
        .filter(|line| !line.starts_with(&format!("{token}\t")))
        .collect::<Vec<_>>()
        .join("\n");
    let _ = fs::write(path, format!("{next}\n"));
}

fn secure_cookie_suffix(cfg: &Config) -> &'static str {
    if cfg.public_base_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    }
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
