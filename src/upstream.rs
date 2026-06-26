use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::accounts::{self, UsageRecord};
use crate::auth::ApiActor;
use crate::config::{
    Config, PROVIDER_CLAUDE, PROVIDER_CODEX, PROVIDER_OPENCODE_GO, canonical_provider_id,
    default_provider_auth_path, is_opencode_go_messages_model, load_models, load_providers,
    model_id_for_provider, public_model_id, split_model_provider_prefix,
};
use crate::http;
use crate::json::{self, Json};
use crate::util::{base64_url_decode, now_secs};

const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are Codex, a focused software engineering agent. Answer accurately, preserve user intent, and use tools only when provided by the client.";
const DEFAULT_CLAUDE_SYSTEM_PROMPT: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
const DEFAULT_OPENCODE_SYSTEM_PROMPT: &str =
    "You are OpenCode Go, a focused coding assistant. Answer accurately and concisely.";
const DEFAULT_CLAUDE_MAX_TOKENS: u64 = 64_000;

#[derive(Clone, Debug)]
pub struct CurlResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
struct CodexAuth {
    access_token: String,
    refresh_token: String,
    account_id: String,
}

#[derive(Clone, Debug)]
struct ClaudeAuth {
    access_token: String,
}

#[derive(Clone, Debug)]
struct OpenCodeGoAuth {
    api_key: String,
}

pub fn models_json(cfg: &Config) -> String {
    let mut data = Vec::new();
    for model in load_models(cfg).into_iter().filter(|m| m.enabled) {
        let mut obj = Json::object();
        obj.set("id", Json::String(public_model_id(&model)));
        obj.set("object", Json::String("model".to_string()));
        obj.set("owned_by", Json::String(model.provider_id));
        data.push(obj);
    }
    let mut root = Json::object();
    root.set("object", Json::String("list".to_string()));
    root.set("data", Json::Array(data));
    root.stringify()
}

pub fn forward_model(req: &http::Request, stream: &mut TcpStream, cfg: &Config, actor: &ApiActor) {
    let provider_id = request_provider_id(req, cfg);
    if !provider_enabled(cfg, &provider_id) {
        let _ = http::send_json(
            stream,
            503,
            &error_json(&format!("provider `{provider_id}` is disabled")),
        );
        return;
    }
    match provider_id.as_str() {
        PROVIDER_CLAUDE => forward_claude(req, stream, cfg, actor),
        PROVIDER_OPENCODE_GO => forward_opencode_go(req, stream, cfg, actor),
        _ => forward_codex(req, stream, cfg, actor),
    }
}

pub fn forward_codex(req: &http::Request, stream: &mut TcpStream, cfg: &Config, actor: &ApiActor) {
    let requested_model = request_model(req, cfg);
    let body = match transform_request(&req.path, &req.body, cfg) {
        Ok(body) => body,
        Err(e) => {
            let _ = http::send_json(stream, 400, &error_json(&e));
            return;
        }
    };

    let auth = match load_or_refresh_codex_auth(cfg) {
        Ok(a) => a,
        Err(e) => {
            let _ = http::send_json(stream, 502, &error_json(&e));
            return;
        }
    };

    let session_id = req
        .header("x-session-id")
        .or_else(|| req.header("session_id"))
        .unwrap_or("akurai-router")
        .to_string();
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "text/event-stream".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {}", auth.access_token),
        ),
        ("originator".to_string(), "codex_cli_rs".to_string()),
        ("User-Agent".to_string(), "codex_cli_rs/0.136.0".to_string()),
        ("session_id".to_string(), session_id),
    ];
    if !auth.account_id.is_empty() {
        headers.push(("chatgpt-account-id".to_string(), auth.account_id));
    }

    match curl_stream_post(&cfg.codex_responses_url, &headers, body.as_bytes(), stream) {
        Ok(status) => record_usage(
            cfg,
            actor,
            PROVIDER_CODEX,
            &requested_model,
            &req.path,
            status,
            None,
        ),
        Err(e) => {
            record_usage(
                cfg,
                actor,
                PROVIDER_CODEX,
                &requested_model,
                &req.path,
                502,
                None,
            );
            let _ = http::send_json(stream, 502, &error_json(&e));
        }
    }
}

fn forward_claude(req: &http::Request, stream: &mut TcpStream, cfg: &Config, actor: &ApiActor) {
    let requested_model = request_model(req, cfg);
    let body = match transform_claude_request(&req.body, cfg) {
        Ok(body) => body,
        Err(e) => {
            let _ = http::send_json(stream, 400, &error_json(&e));
            return;
        }
    };

    let auth = match load_or_refresh_claude_auth(cfg) {
        Ok(a) => a,
        Err(e) => {
            let _ = http::send_json(stream, 502, &error_json(&e));
            return;
        }
    };

    let session_id = req
        .header("x-session-id")
        .or_else(|| req.header("session_id"))
        .unwrap_or("akurai-router")
        .to_string();
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {}", auth.access_token),
        ),
        (
            "Anthropic-Version".to_string(),
            "2023-06-01".to_string(),
        ),
        (
            "Anthropic-Beta".to_string(),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advanced-tool-use-2025-11-20,effort-2025-11-24,structured-outputs-2025-12-15,fast-mode-2026-02-01,redact-thinking-2026-02-12,token-efficient-tools-2026-03-28".to_string(),
        ),
        (
            "Anthropic-Dangerous-Direct-Browser-Access".to_string(),
            "true".to_string(),
        ),
        ("User-Agent".to_string(), "claude-cli/2.1.92 (external, sdk-cli)".to_string()),
        ("X-App".to_string(), "cli".to_string()),
        ("X-Stainless-Helper-Method".to_string(), "stream".to_string()),
        ("X-Stainless-Retry-Count".to_string(), "0".to_string()),
        ("X-Stainless-Runtime-Version".to_string(), "v24.14.0".to_string()),
        ("X-Stainless-Package-Version".to_string(), "0.80.0".to_string()),
        ("X-Stainless-Runtime".to_string(), "node".to_string()),
        ("X-Stainless-Lang".to_string(), "js".to_string()),
        ("X-Stainless-Arch".to_string(), runtime_arch().to_string()),
        ("X-Stainless-Os".to_string(), runtime_os().to_string()),
        ("X-Stainless-Timeout".to_string(), "600".to_string()),
        ("X-Session-Id".to_string(), session_id),
    ];
    let headers_ref: Vec<(&str, String)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    match curl_capture(
        "POST",
        &cfg.claude_messages_url,
        &headers_ref,
        body.as_bytes(),
        900,
    ) {
        Ok(resp) => {
            let text = String::from_utf8_lossy(&resp.body);
            record_usage(
                cfg,
                actor,
                PROVIDER_CLAUDE,
                &requested_model,
                &req.path,
                resp.status,
                Some(&text),
            );
            if resp.status != 200 {
                let _ = http::send_json(
                    stream,
                    resp.status,
                    &error_json(&String::from_utf8_lossy(&resp.body)),
                );
                return;
            }
            let text = String::from_utf8_lossy(&resp.body);
            match claude_to_openai_response(&text, cfg) {
                Ok(json) => {
                    let _ = http::send_json(stream, 200, &json);
                }
                Err(e) => {
                    let _ = http::send_json(stream, 502, &error_json(&e));
                }
            }
        }
        Err(e) => {
            record_usage(
                cfg,
                actor,
                PROVIDER_CLAUDE,
                &requested_model,
                &req.path,
                502,
                None,
            );
            let _ = http::send_json(stream, 502, &error_json(&e));
        }
    }
}

fn forward_opencode_go(
    req: &http::Request,
    stream: &mut TcpStream,
    cfg: &Config,
    actor: &ApiActor,
) {
    if !req.path.ends_with("/chat/completions") {
        let _ = http::send_json(
            stream,
            400,
            &error_json("opencode-go models support /v1/chat/completions"),
        );
        return;
    }
    let input = match json::parse(&String::from_utf8_lossy(&req.body)) {
        Ok(value) => value,
        Err(e) => {
            let _ = http::send_json(stream, 400, &error_json(&e));
            return;
        }
    };
    let requested_model = input.get_str("model").unwrap_or("glm-5.2").to_string();
    let upstream_model = resolve_model_id_for_provider(cfg, &requested_model, PROVIDER_OPENCODE_GO);
    if is_opencode_go_messages_model(&upstream_model) {
        forward_opencode_go_messages(req, stream, cfg, actor);
        return;
    }

    let mut body = input;
    body.set("model", Json::String(upstream_model));
    let auth = match load_opencode_go_auth(cfg) {
        Ok(a) => a,
        Err(e) => {
            let _ = http::send_json(stream, 502, &error_json(&e));
            return;
        }
    };
    let accept = if body.get_bool("stream").unwrap_or(false) {
        "text/event-stream"
    } else {
        "application/json"
    };
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), accept.to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {}", auth.api_key),
        ),
    ];
    let body_text = body.stringify();
    if body.get_bool("stream").unwrap_or(false) {
        match curl_stream_post(
            &cfg.opencode_go_chat_url,
            &headers,
            body_text.as_bytes(),
            stream,
        ) {
            Ok(status) => record_usage(
                cfg,
                actor,
                PROVIDER_OPENCODE_GO,
                &requested_model,
                &req.path,
                status,
                None,
            ),
            Err(e) => {
                record_usage(
                    cfg,
                    actor,
                    PROVIDER_OPENCODE_GO,
                    &requested_model,
                    &req.path,
                    502,
                    None,
                );
                let _ = http::send_json(stream, 502, &error_json(&e));
            }
        }
    } else {
        let headers_ref: Vec<(&str, String)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        match curl_capture(
            "POST",
            &cfg.opencode_go_chat_url,
            &headers_ref,
            body_text.as_bytes(),
            900,
        ) {
            Ok(resp) => {
                let text = String::from_utf8_lossy(&resp.body);
                record_usage(
                    cfg,
                    actor,
                    PROVIDER_OPENCODE_GO,
                    &requested_model,
                    &req.path,
                    resp.status,
                    Some(&text),
                );
                let _ = http::send_json(stream, resp.status, &text);
            }
            Err(e) => {
                record_usage(
                    cfg,
                    actor,
                    PROVIDER_OPENCODE_GO,
                    &requested_model,
                    &req.path,
                    502,
                    None,
                );
                let _ = http::send_json(stream, 502, &error_json(&e));
            }
        }
    }
}

fn forward_opencode_go_messages(
    req: &http::Request,
    stream: &mut TcpStream,
    cfg: &Config,
    actor: &ApiActor,
) {
    let requested_model = request_model(req, cfg);
    let body = match transform_anthropic_messages_request(
        &req.body,
        cfg,
        PROVIDER_OPENCODE_GO,
        DEFAULT_OPENCODE_SYSTEM_PROMPT,
    ) {
        Ok(body) => body,
        Err(e) => {
            let _ = http::send_json(stream, 400, &error_json(&e));
            return;
        }
    };

    let auth = match load_opencode_go_auth(cfg) {
        Ok(a) => a,
        Err(e) => {
            let _ = http::send_json(stream, 502, &error_json(&e));
            return;
        }
    };
    let headers = vec![
        ("Content-Type", "application/json".to_string()),
        ("Accept", "application/json".to_string()),
        ("x-api-key", auth.api_key),
        ("anthropic-version", "2023-06-01".to_string()),
    ];
    match curl_capture(
        "POST",
        &cfg.opencode_go_messages_url,
        &headers,
        body.as_bytes(),
        900,
    ) {
        Ok(resp) => {
            let text = String::from_utf8_lossy(&resp.body);
            record_usage(
                cfg,
                actor,
                PROVIDER_OPENCODE_GO,
                &requested_model,
                &req.path,
                resp.status,
                Some(&text),
            );
            if resp.status != 200 {
                let _ = http::send_json(
                    stream,
                    resp.status,
                    &error_json(&String::from_utf8_lossy(&resp.body)),
                );
                return;
            }
            match claude_to_openai_response(&text, cfg) {
                Ok(json) => {
                    let _ = http::send_json(stream, 200, &json);
                }
                Err(e) => {
                    let _ = http::send_json(stream, 502, &error_json(&e));
                }
            }
        }
        Err(e) => {
            record_usage(
                cfg,
                actor,
                PROVIDER_OPENCODE_GO,
                &requested_model,
                &req.path,
                502,
                None,
            );
            let _ = http::send_json(stream, 502, &error_json(&e));
        }
    }
}

pub fn fetch_codex_models(cfg: &Config) -> Result<CurlResponse, String> {
    let auth = load_or_refresh_codex_auth(cfg)?;
    let mut headers = vec![
        ("Accept", "application/json".to_string()),
        ("Authorization", format!("Bearer {}", auth.access_token)),
        ("originator", "codex_cli_rs".to_string()),
        ("User-Agent", "codex_cli_rs/0.136.0".to_string()),
    ];
    if !auth.account_id.is_empty() {
        headers.push(("chatgpt-account-id", auth.account_id));
    }
    curl_capture("GET", &cfg.codex_models_url, &headers, b"", 30)
}

pub fn fetch_claude_models(cfg: &Config) -> Result<CurlResponse, String> {
    let auth = load_or_refresh_claude_auth(cfg)?;
    let headers = vec![
        ("Accept", "application/json".to_string()),
        ("Authorization", format!("Bearer {}", auth.access_token)),
        ("Anthropic-Version", "2023-06-01".to_string()),
        ("Anthropic-Beta", "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advanced-tool-use-2025-11-20,effort-2025-11-24,structured-outputs-2025-12-15,fast-mode-2026-02-01,redact-thinking-2026-02-12,token-efficient-tools-2026-03-28".to_string()),
    ];
    curl_capture("GET", &cfg.claude_models_url, &headers, b"", 30)
}

pub fn fetch_opencode_go_models(cfg: &Config) -> Result<CurlResponse, String> {
    let auth = load_opencode_go_auth(cfg)?;
    let headers = vec![
        ("Accept", "application/json".to_string()),
        ("Authorization", format!("Bearer {}", auth.api_key)),
    ];
    curl_capture("GET", &cfg.opencode_go_models_url, &headers, b"", 30)
}

fn record_usage(
    cfg: &Config,
    actor: &ApiActor,
    provider_id: &str,
    model: &str,
    endpoint: &str,
    status: u16,
    response_body: Option<&str>,
) {
    let (prompt_tokens, completion_tokens, total_tokens, cost_usd) =
        usage_metrics(response_body.unwrap_or(""));
    let _ = accounts::record_usage(
        cfg,
        &UsageRecord {
            ts: now_secs(),
            email: actor.email.clone(),
            key_id: actor.key_id.clone(),
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            endpoint: endpoint.to_string(),
            status,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cost_usd,
        },
    );
}

fn usage_metrics(text: &str) -> (u64, u64, u64, f64) {
    let Ok(root) = json::parse(text) else {
        return (0, 0, 0, 0.0);
    };
    let cost = json_f64(root.get("cost")).unwrap_or(0.0);
    let Some(usage) = root.get("usage") else {
        return (0, 0, 0, cost);
    };
    let prompt = json_u64(usage.get("prompt_tokens"))
        .or_else(|| json_u64(usage.get("input_tokens")))
        .unwrap_or(0);
    let completion = json_u64(usage.get("completion_tokens"))
        .or_else(|| json_u64(usage.get("output_tokens")))
        .unwrap_or(0);
    let total = json_u64(usage.get("total_tokens")).unwrap_or(prompt + completion);
    (prompt, completion, total, cost)
}

fn json_u64(value: Option<&Json>) -> Option<u64> {
    match value? {
        Json::Number(n) => n.parse().ok(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn json_f64(value: Option<&Json>) -> Option<f64> {
    match value? {
        Json::Number(n) => n.parse::<f64>().ok(),
        Json::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
    .filter(|v| v.is_finite())
}

pub fn curl_capture(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: &[u8],
    timeout_secs: u64,
) -> Result<CurlResponse, String> {
    let mut cmd = Command::new("curl");
    cmd.arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg(timeout_secs.to_string())
        .arg("-X")
        .arg(method)
        .arg("-D")
        .arg("-")
        .arg("-o")
        .arg("-")
        .arg(url);
    for (key, value) in headers {
        cmd.arg("-H").arg(format!("{key}: {value}"));
    }
    if method != "GET" {
        cmd.arg("--data-binary").arg("@-");
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn curl: {e}"))?;
    if method != "GET" {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(body).map_err(|e| e.to_string())?;
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_curl_output(&out.stdout)
}

fn curl_stream_post(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    stream: &mut TcpStream,
) -> Result<u16, String> {
    let mut cmd = Command::new("curl");
    cmd.arg("--silent")
        .arg("--show-error")
        .arg("--no-buffer")
        .arg("--http1.1")
        .arg("--max-time")
        .arg("900")
        .arg("-X")
        .arg("POST")
        .arg("-D")
        .arg("-")
        .arg("-o")
        .arg("-")
        .arg(url);
    for (key, value) in headers {
        cmd.arg("-H").arg(format!("{key}: {value}"));
    }
    cmd.arg("--data-binary").arg("@-");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn curl: {e}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "curl stdin unavailable".to_string())?;
        stdin.write_all(body).map_err(|e| e.to_string())?;
    }

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "curl stdout unavailable".to_string())?;
    let mut header_buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let (status, response_headers, body_prefix) = loop {
        let n = stdout.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("upstream closed before response headers".to_string());
        }
        header_buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&header_buf) {
            let head = header_buf[..pos].to_vec();
            let prefix = header_buf[pos + 4..].to_vec();
            let (status, headers) = parse_headers(&head)?;
            break (status, headers, prefix);
        }
        if header_buf.len() > 128 * 1024 {
            return Err("upstream header too large".to_string());
        }
    };

    http::stream_headers(stream, status, &response_headers).map_err(|e| e.to_string())?;
    if !body_prefix.is_empty() {
        stream.write_all(&body_prefix).map_err(|e| e.to_string())?;
    }
    loop {
        let n = stdout.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        stream.write_all(&tmp[..n]).map_err(|e| e.to_string())?;
        stream.flush().map_err(|e| e.to_string())?;
    }
    let _ = child.wait();
    Ok(status)
}

fn transform_request(path: &str, raw: &[u8], cfg: &Config) -> Result<String, String> {
    let input = String::from_utf8_lossy(raw);
    let value = json::parse(&input)?;
    let mut body = if path.ends_with("/chat/completions") {
        chat_to_responses(value, cfg)?
    } else {
        value
    };
    normalize_codex_body(&mut body, cfg)?;
    Ok(body.stringify())
}

fn chat_to_responses(value: Json, cfg: &Config) -> Result<Json, String> {
    let mut out = Json::object();
    let model = value
        .get_str("model")
        .unwrap_or(&cfg.default_model)
        .strip_prefix("cx/")
        .or_else(|| {
            value
                .get_str("model")
                .and_then(|m| m.strip_prefix("codex/"))
        })
        .unwrap_or_else(|| value.get_str("model").unwrap_or(&cfg.default_model))
        .to_string();
    out.set("model", Json::String(model));
    if let Some(stream) = value.get_bool("stream") {
        out.set("stream", Json::Bool(stream));
    }
    if let Some(v) = value.get("tools") {
        out.set("tools", v.clone());
    }
    if let Some(v) = value.get("tool_choice") {
        out.set("tool_choice", v.clone());
    }
    if let Some(v) = value.get("reasoning_effort") {
        out.set("reasoning_effort", v.clone());
    }

    let mut input = Vec::new();
    let Some(Json::Array(messages)) = value.get("messages") else {
        return Err("chat/completions request requires messages[]".to_string());
    };
    for msg in messages {
        let role = msg
            .get_str("role")
            .unwrap_or("user")
            .replace("system", "developer");
        let parts = chat_content_to_parts(msg.get("content"));
        let mut item = Json::object();
        item.set("type", Json::String("message".to_string()));
        item.set("role", Json::String(role));
        item.set("content", Json::Array(parts));
        input.push(item);
    }
    out.set("input", Json::Array(input));
    Ok(out)
}

/// Convert an OpenAI chat-completions `content` field into Responses-API content
/// parts. Text is preserved as `input_text`; images are forwarded as
/// `input_image` (previously every non-text part was silently dropped, so the
/// upstream model never saw screenshots).
fn chat_content_to_parts(content: Option<&Json>) -> Vec<Json> {
    let mut parts = Vec::new();
    match content {
        Some(Json::String(s)) => parts.push(input_text_part(s)),
        Some(Json::Array(items)) => {
            for part in items {
                let kind = part.get_str("type").unwrap_or("");
                if kind == "image_url" || part.get("image_url").is_some() {
                    if let Some(image) = input_image_part(part) {
                        parts.push(image);
                    }
                } else if let Some(text) = part.get_str("text") {
                    if !text.is_empty() {
                        parts.push(input_text_part(text));
                    }
                }
            }
        }
        _ => {}
    }
    if parts.is_empty() {
        parts.push(input_text_part("..."));
    }
    parts
}

fn input_text_part(text: &str) -> Json {
    let mut content = Json::object();
    content.set("type", Json::String("input_text".to_string()));
    content.set(
        "text",
        Json::String(if text.is_empty() {
            "...".to_string()
        } else {
            text.to_string()
        }),
    );
    content
}

/// Map an OpenAI `{"type":"image_url","image_url":{"url":...,"detail":...}}`
/// part (or the bare-string `image_url` form) to a Responses-API `input_image`
/// content item. Accepts `https://` URLs and `data:` URIs (e.g. screenshots).
fn input_image_part(part: &Json) -> Option<Json> {
    let (url, detail) = match part.get("image_url") {
        Some(Json::String(s)) => (s.clone(), None),
        Some(obj @ Json::Object(_)) => (
            obj.get_str("url")?.to_string(),
            obj.get_str("detail").map(|s| s.to_string()),
        ),
        _ => return None,
    };
    if url.is_empty() {
        return None;
    }
    let mut content = Json::object();
    content.set("type", Json::String("input_image".to_string()));
    content.set("image_url", Json::String(url));
    content.set(
        "detail",
        Json::String(detail.unwrap_or_else(|| "auto".to_string())),
    );
    Some(content)
}

fn normalize_codex_body(body: &mut Json, cfg: &Config) -> Result<(), String> {
    let model = body
        .get_str("model")
        .unwrap_or(&cfg.default_model)
        .to_string();
    let model = model_id_for_provider(&model, PROVIDER_CODEX);
    body.set("model", Json::String(resolve_model_id(cfg, &model)));
    body.set("stream", Json::Bool(true));
    body.set("store", Json::Bool(false));
    if body.get_str("instructions").unwrap_or("").trim().is_empty() {
        body.set(
            "instructions",
            Json::String(DEFAULT_CODEX_INSTRUCTIONS.to_string()),
        );
    }
    normalize_input(body);
    normalize_reasoning(body);
    for key in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "logprobs",
        "top_logprobs",
        "n",
        "seed",
        "max_tokens",
        "max_completion_tokens",
        "max_output_tokens",
        "user",
        "metadata",
        "stream_options",
        "previous_response_id",
        "reasoning_effort",
    ] {
        body.remove(key);
    }
    let allowed = [
        "model",
        "input",
        "instructions",
        "tools",
        "tool_choice",
        "stream",
        "store",
        "reasoning",
        "service_tier",
        "include",
        "prompt_cache_key",
        "client_metadata",
        "text",
    ];
    if let Json::Object(items) = body {
        items.retain(|(k, _)| allowed.contains(&k.as_str()));
    }
    Ok(())
}

fn transform_claude_request(raw: &[u8], cfg: &Config) -> Result<String, String> {
    transform_anthropic_messages_request(raw, cfg, PROVIDER_CLAUDE, DEFAULT_CLAUDE_SYSTEM_PROMPT)
}

fn transform_anthropic_messages_request(
    raw: &[u8],
    cfg: &Config,
    provider_id: &str,
    default_system_prompt: &str,
) -> Result<String, String> {
    let input = json::parse(&String::from_utf8_lossy(raw))?;
    let mut body = Json::object();
    let model = input
        .get_str("model")
        .unwrap_or(&cfg.default_model)
        .to_string();
    body.set(
        "model",
        Json::String(resolve_model_id_for_provider(cfg, &model, provider_id)),
    );
    body.set(
        "max_tokens",
        Json::Number(
            number_from_json(input.get("max_tokens"))
                .unwrap_or(DEFAULT_CLAUDE_MAX_TOKENS)
                .to_string(),
        ),
    );
    body.set("stream", Json::Bool(false));
    if let Some(v) = input.get("temperature") {
        body.set("temperature", v.clone());
    }
    let (system_blocks, messages) = claude_messages_from_openai(&input, default_system_prompt);
    body.set("messages", Json::Array(messages));
    if system_blocks.is_empty() {
        body.set(
            "system",
            Json::Array(vec![claude_text_block(default_system_prompt)]),
        );
    } else {
        body.set("system", Json::Array(system_blocks));
    }
    if let Some(v) = input.get("top_p") {
        body.set("top_p", v.clone());
    }
    Ok(body.stringify())
}

fn claude_messages_from_openai(
    input: &Json,
    default_system_prompt: &str,
) -> (Vec<Json>, Vec<Json>) {
    let mut system_blocks = vec![claude_text_block(default_system_prompt)];
    let mut messages = Vec::new();

    if let Some(Json::Array(values)) = input.get("messages") {
        let mut current_role: Option<String> = None;
        let mut current_text = String::new();
        for msg in values {
            let role = msg.get_str("role").unwrap_or("user");
            let text = extract_openai_text(msg.get("content"));
            if role == "system" {
                if !text.trim().is_empty() {
                    system_blocks.push(claude_text_block(&text));
                }
                continue;
            }
            let normalized = if role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            if current_role.as_deref() != Some(normalized) {
                if let Some(prev_role) = current_role.take() {
                    push_claude_message(&mut messages, &prev_role, current_text.clone());
                }
                current_role = Some(normalized.to_string());
                current_text.clear();
            }
            if !text.is_empty() {
                if !current_text.is_empty() {
                    current_text.push('\n');
                }
                current_text.push_str(&text);
            }
        }
        if let Some(prev_role) = current_role {
            push_claude_message(&mut messages, &prev_role, current_text);
        }
    } else if let Some(input_value) = input.get("input") {
        let text = extract_openai_text(Some(input_value));
        push_claude_message(&mut messages, "user", text);
    }

    if messages.is_empty() {
        push_claude_message(&mut messages, "user", "...".to_string());
    }
    (system_blocks, messages)
}

fn push_claude_message(messages: &mut Vec<Json>, role: &str, text: String) {
    if text.trim().is_empty() {
        return;
    }
    let mut item = Json::object();
    item.set(
        "role",
        Json::String(if role == "assistant" {
            "assistant".to_string()
        } else {
            "user".to_string()
        }),
    );
    item.set("content", Json::Array(vec![claude_text_block(&text)]));
    messages.push(item);
}

fn extract_openai_text(value: Option<&Json>) -> String {
    match value {
        Some(Json::String(text)) => text.clone(),
        Some(Json::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get_str("text"))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Json::Object(_)) => value
            .and_then(|v| v.get_str("text"))
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn claude_text_block(text: &str) -> Json {
    let mut block = Json::object();
    block.set("type", Json::String("text".to_string()));
    block.set("text", Json::String(text.to_string()));
    block
}

fn claude_to_openai_response(raw: &str, cfg: &Config) -> Result<String, String> {
    let parsed = json::parse(raw)?;
    let mut root = Json::object();
    root.set(
        "id",
        Json::String(
            parsed
                .get_str("id")
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("chatcmpl-{}", now_secs())),
        ),
    );
    root.set("object", Json::String("chat.completion".to_string()));
    root.set("created", Json::Number(now_secs().to_string()));
    root.set(
        "model",
        Json::String(
            parsed
                .get_str("model")
                .unwrap_or(&cfg.default_model)
                .to_string(),
        ),
    );
    let mut choice = Json::object();
    choice.set("index", Json::Number("0".to_string()));
    choice.set(
        "finish_reason",
        Json::String(map_claude_finish_reason(parsed.get_str("stop_reason"))),
    );
    let mut message = Json::object();
    message.set("role", Json::String("assistant".to_string()));
    message.set("content", Json::String(claude_response_text(&parsed)));
    choice.set("message", message);
    root.set("choices", Json::Array(vec![choice]));

    let mut usage = Json::object();
    if let Some(u) = parsed.get("usage") {
        if let Some(input_tokens) = json_number_string(u.get("input_tokens")) {
            usage.set("prompt_tokens", Json::Number(input_tokens));
        }
        if let Some(output_tokens) = json_number_string(u.get("output_tokens")) {
            usage.set("completion_tokens", Json::Number(output_tokens));
        }
    }
    let prompt_tokens = json_number_string(usage.get("prompt_tokens"));
    let completion_tokens = json_number_string(usage.get("completion_tokens"));
    if let (Some(p), Some(c)) = (prompt_tokens, completion_tokens) {
        if let (Ok(pn), Ok(cn)) = (p.parse::<u64>(), c.parse::<u64>()) {
            usage.set("total_tokens", Json::Number((pn + cn).to_string()));
        }
    }
    root.set("usage", usage);
    Ok(root.stringify())
}

fn claude_response_text(parsed: &Json) -> String {
    match parsed.get("content") {
        Some(Json::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get_str("text"))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn map_claude_finish_reason(reason: Option<&str>) -> String {
    match reason.unwrap_or("end_turn") {
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "stop_sequence" => "stop".to_string(),
        _ => "stop".to_string(),
    }
}

fn json_number_string(value: Option<&Json>) -> Option<String> {
    match value? {
        Json::Number(n) => Some(n.clone()),
        Json::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn number_from_json(value: Option<&Json>) -> Option<u64> {
    match value? {
        Json::Number(n) => n.parse().ok(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn resolve_model_id_for_provider(cfg: &Config, model: &str, provider_id: &str) -> String {
    let provider_id = canonical_provider_id(provider_id);
    let model = model_id_for_provider(model, &provider_id);
    for configured in load_models(cfg) {
        if canonical_provider_id(&configured.provider_id) == provider_id
            && (configured.id == model || configured.upstream_id == model)
        {
            return configured.upstream_id;
        }
    }
    model
}

fn runtime_os() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "MacOS",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        other => other,
    }
}

fn runtime_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        other => other,
    }
}

fn normalize_input(body: &mut Json) {
    match body.get("input") {
        Some(Json::Array(values)) if !values.is_empty() => {}
        Some(Json::String(text)) => {
            let mut item = Json::object();
            item.set("type", Json::String("message".to_string()));
            item.set("role", Json::String("user".to_string()));
            let mut content = Json::object();
            content.set("type", Json::String("input_text".to_string()));
            content.set("text", Json::String(text.clone()));
            item.set("content", Json::Array(vec![content]));
            body.set("input", Json::Array(vec![item]));
        }
        _ => {
            let mut item = Json::object();
            item.set("type", Json::String("message".to_string()));
            item.set("role", Json::String("user".to_string()));
            let mut content = Json::object();
            content.set("type", Json::String("input_text".to_string()));
            content.set("text", Json::String("...".to_string()));
            item.set("content", Json::Array(vec![content]));
            body.set("input", Json::Array(vec![item]));
        }
    }
    if let Some(Json::Array(items)) = body.get_mut("input") {
        for item in items {
            if let Json::Object(fields) = item {
                if let Some((_, Json::String(role))) = fields.iter_mut().find(|(k, _)| k == "role")
                {
                    if role == "system" {
                        *role = "developer".to_string();
                    }
                }
                fields.retain(|(k, v)| {
                    !(k == "id" && matches!(v, Json::String(s) if is_server_id(s)))
                });
            }
        }
    }
}

fn is_server_id(value: &str) -> bool {
    ["rs_", "fc_", "resp_", "msg_"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

fn normalize_reasoning(body: &mut Json) {
    let effort = body
        .get_str("reasoning_effort")
        .map(|s| s.to_string())
        .unwrap_or_else(|| infer_effort(body.get_str("model").unwrap_or("")));
    if body.get("reasoning").is_none() {
        let mut reasoning = Json::object();
        reasoning.set("effort", Json::String(effort.clone()));
        reasoning.set("summary", Json::String("auto".to_string()));
        body.set("reasoning", reasoning);
    }
    if effort != "none" {
        body.set(
            "include",
            Json::Array(vec![Json::String(
                "reasoning.encrypted_content".to_string(),
            )]),
        );
    }
}

fn infer_effort(model: &str) -> String {
    for effort in ["none", "low", "medium", "high", "xhigh"] {
        if model.ends_with(&format!("-{effort}")) {
            return effort.to_string();
        }
    }
    "low".to_string()
}

fn resolve_model_id(cfg: &Config, model: &str) -> String {
    let model = model_id_for_provider(model, PROVIDER_CODEX);
    let mut id = model.to_string();
    for effort in ["-none", "-low", "-medium", "-high", "-xhigh"] {
        if id.ends_with(effort) {
            id.truncate(id.len() - effort.len());
        }
    }
    for configured in load_models(cfg) {
        if canonical_provider_id(&configured.provider_id) == PROVIDER_CODEX
            && configured.id == model
        {
            return configured.upstream_id;
        }
    }
    id
}

fn request_provider_id(req: &http::Request, cfg: &Config) -> String {
    provider_for_model(cfg, &request_model(req, cfg))
}

fn request_model(req: &http::Request, cfg: &Config) -> String {
    let raw = String::from_utf8_lossy(&req.body);
    if let Ok(root) = json::parse(&raw) {
        if let Some(model) = root.get_str("model") {
            return model.to_string();
        }
    }
    cfg.default_model.clone()
}

fn provider_for_model(cfg: &Config, model: &str) -> String {
    if let Some((provider_id, _)) = split_model_provider_prefix(model) {
        return provider_id;
    }
    for configured in load_models(cfg) {
        if configured.id == model || configured.upstream_id == model {
            return canonical_provider_id(&configured.provider_id);
        }
    }
    if model.starts_with("claude-") {
        PROVIDER_CLAUDE.to_string()
    } else if crate::config::is_opencode_go_model(model) {
        PROVIDER_OPENCODE_GO.to_string()
    } else {
        PROVIDER_CODEX.to_string()
    }
}

fn provider_enabled(cfg: &Config, provider_id: &str) -> bool {
    let provider_id = canonical_provider_id(provider_id);
    load_providers(cfg)
        .into_iter()
        .find(|p| canonical_provider_id(&p.id) == provider_id)
        .map(|p| p.enabled)
        .unwrap_or(true)
}

fn load_or_refresh_codex_auth(cfg: &Config) -> Result<CodexAuth, String> {
    let mut root = read_codex_auth(cfg)?;
    let mut auth = extract_codex_auth(&root)?;
    if token_expiring_soon(&auth.access_token) {
        let refreshed = refresh_codex_token(&auth.refresh_token)?;
        merge_refreshed_tokens(&mut root, &refreshed)?;
        write_codex_auth(cfg, &root)?;
        auth = extract_codex_auth(&root)?;
    }
    Ok(auth)
}

fn read_codex_auth(cfg: &Config) -> Result<Json, String> {
    let path = provider_auth_path(cfg, PROVIDER_CODEX);
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read Codex auth at {}: {e}", path.display()))?;
    json::parse(&text).map_err(|e| format!("failed to parse Codex auth JSON: {e}"))
}

fn load_or_refresh_claude_auth(cfg: &Config) -> Result<ClaudeAuth, String> {
    let root = read_claude_auth(cfg)?;
    extract_claude_auth(&root)
}

fn load_opencode_go_auth(cfg: &Config) -> Result<OpenCodeGoAuth, String> {
    let path = provider_auth_path(cfg, PROVIDER_OPENCODE_GO);
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read OpenCode Go auth at {}: {e}", path.display()))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("OpenCode Go auth file is empty".to_string());
    }
    if !trimmed.starts_with('{') {
        return Ok(OpenCodeGoAuth {
            api_key: trimmed.to_string(),
        });
    }
    let root =
        json::parse(trimmed).map_err(|e| format!("failed to parse OpenCode auth JSON: {e}"))?;
    extract_opencode_go_auth(&root)
}

fn read_claude_auth(cfg: &Config) -> Result<Json, String> {
    let path = provider_auth_path(cfg, PROVIDER_CLAUDE);
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read Claude auth at {}: {e}", path.display()))?;
    json::parse(&text).map_err(|e| format!("failed to parse Claude auth JSON: {e}"))
}

fn write_codex_auth(cfg: &Config, root: &Json) -> Result<(), String> {
    let path = provider_auth_path(cfg, PROVIDER_CODEX);
    fs::write(&path, root.stringify()).map_err(|e| e.to_string())?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    Ok(())
}

fn provider_auth_path(cfg: &Config, provider_id: &str) -> PathBuf {
    let provider_id = canonical_provider_id(provider_id);
    load_providers(cfg)
        .into_iter()
        .find(|p| canonical_provider_id(&p.id) == provider_id)
        .map(|p| p.auth_path)
        .unwrap_or_else(|| default_provider_auth_path(cfg, &provider_id))
}

fn extract_codex_auth(root: &Json) -> Result<CodexAuth, String> {
    let tokens = root
        .get("tokens")
        .ok_or_else(|| "Codex auth missing tokens object".to_string())?;
    let access_token = tokens.get_str("access_token").unwrap_or("").to_string();
    if access_token.is_empty() {
        return Err("Codex auth missing access_token; run `codex login` on the VM".to_string());
    }
    let refresh_token = tokens.get_str("refresh_token").unwrap_or("").to_string();
    let account_id = tokens
        .get_str("account_id")
        .map(|s| s.to_string())
        .or_else(|| {
            claim_from_jwt(
                &access_token,
                "https://api.openai.com/auth",
                "chatgpt_account_id",
            )
        })
        .unwrap_or_default();
    Ok(CodexAuth {
        access_token,
        refresh_token,
        account_id,
    })
}

fn extract_claude_auth(root: &Json) -> Result<ClaudeAuth, String> {
    let oauth = root
        .get("claudeAiOauth")
        .ok_or_else(|| "Claude auth missing claudeAiOauth object".to_string())?;
    let access_token = oauth.get_str("accessToken").unwrap_or("").to_string();
    if access_token.is_empty() {
        return Err(
            "Claude auth missing accessToken; run `claude` on the source machine".to_string(),
        );
    }
    Ok(ClaudeAuth { access_token })
}

fn extract_opencode_go_auth(root: &Json) -> Result<OpenCodeGoAuth, String> {
    for provider_id in [PROVIDER_OPENCODE_GO, "opencode"] {
        if let Some(provider) = root.get(provider_id) {
            for key_name in ["key", "apiKey", "api_key", "accessToken", "access_token"] {
                if let Some(key) = provider.get_str(key_name) {
                    if !key.trim().is_empty() {
                        return Ok(OpenCodeGoAuth {
                            api_key: key.trim().to_string(),
                        });
                    }
                }
            }
        }
    }
    for key_name in ["apiKey", "api_key", "key"] {
        if let Some(key) = root.get_str(key_name) {
            if !key.trim().is_empty() {
                return Ok(OpenCodeGoAuth {
                    api_key: key.trim().to_string(),
                });
            }
        }
    }
    Err("OpenCode auth missing opencode-go.key; run `opencode auth login` or configure an API key file".to_string())
}

fn token_expiring_soon(access_token: &str) -> bool {
    let Some(exp) = numeric_claim(access_token, "exp") else {
        return false;
    };
    exp <= now_secs() + 300
}

fn refresh_codex_token(refresh_token: &str) -> Result<Json, String> {
    if refresh_token.is_empty() {
        return Err("Codex access token is expiring and no refresh_token is available".to_string());
    }
    let body = format!(
        "{{\"client_id\":\"app_EMoamEEZ73f0CkXaXp7hrann\",\"grant_type\":\"refresh_token\",\"refresh_token\":\"{}\"}}",
        json::escape(refresh_token)
    );
    let response = curl_capture(
        "POST",
        "https://auth.openai.com/oauth/token",
        &[
            ("Content-Type", "application/json".to_string()),
            ("Accept", "application/json".to_string()),
        ],
        body.as_bytes(),
        30,
    )?;
    if response.status != 200 {
        return Err(format!(
            "Codex token refresh failed: {}",
            String::from_utf8_lossy(&response.body)
        ));
    }
    json::parse(&String::from_utf8_lossy(&response.body)).map_err(|e| e.to_string())
}

fn merge_refreshed_tokens(root: &mut Json, refreshed: &Json) -> Result<(), String> {
    let tokens = root
        .get_mut("tokens")
        .ok_or_else(|| "Codex auth missing tokens object".to_string())?;
    for (from, to) in [
        ("access_token", "access_token"),
        ("refresh_token", "refresh_token"),
        ("id_token", "id_token"),
    ] {
        if let Some(value) = refreshed.get_str(from) {
            tokens.set(to, Json::String(value.to_string()));
        }
    }
    root.set("last_refresh", Json::String(now_secs().to_string()));
    Ok(())
}

fn numeric_claim(jwt: &str, key: &str) -> Option<u64> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64_url_decode(payload)?;
    let json = json::parse(&String::from_utf8_lossy(&bytes)).ok()?;
    match json.get(key)? {
        Json::Number(n) => n.parse().ok(),
        _ => None,
    }
}

fn claim_from_jwt(jwt: &str, object_key: &str, key: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64_url_decode(payload)?;
    let json = json::parse(&String::from_utf8_lossy(&bytes)).ok()?;
    json.get(object_key)?.get_str(key).map(|s| s.to_string())
}

fn parse_curl_output(out: &[u8]) -> Result<CurlResponse, String> {
    let pos =
        find_header_end(out).ok_or_else(|| "curl output missing response headers".to_string())?;
    let (status, _) = parse_headers(&out[..pos])?;
    Ok(CurlResponse {
        status,
        body: out[pos + 4..].to_vec(),
    })
}

fn parse_headers(head: &[u8]) -> Result<(u16, Vec<(String, String)>), String> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| "empty upstream response".to_string())?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(502);
    let mut headers = Vec::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok((status, headers))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn error_json(message: &str) -> String {
    let mut err = Json::object();
    err.set("message", Json::String(message.to_string()));
    err.set("type", Json::String("akurai_router_error".to_string()));
    let mut root = Json::object();
    root.set("error", err);
    root.stringify()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ensure_default_files;
    use std::path::PathBuf;

    fn test_config(name: &str) -> Config {
        let data_dir = std::env::temp_dir().join(format!(
            "akurai-router-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&data_dir);
        std::fs::create_dir_all(&data_dir).unwrap();
        Config {
            listen_addr: "127.0.0.1:0".to_string(),
            public_base_url: "http://127.0.0.1:0".to_string(),
            data_dir,
            api_key: "akr_test".to_string(),
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
            admin_allowed_email: "user@example.com".to_string(),
            cookie_secret: "012345678901234567890123456789".to_string(),
        }
    }

    #[test]
    fn forwards_image_url_as_input_image() {
        let content = json::parse(
            r#"[{"type":"text","text":"what is this"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA","detail":"high"}}]"#,
        )
        .unwrap();
        let parts = chat_content_to_parts(Some(&content));
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].get_str("type"), Some("input_text"));
        assert_eq!(parts[0].get_str("text"), Some("what is this"));
        assert_eq!(parts[1].get_str("type"), Some("input_image"));
        assert_eq!(
            parts[1].get_str("image_url"),
            Some("data:image/png;base64,AAAA")
        );
        assert_eq!(parts[1].get_str("detail"), Some("high"));
    }

    #[test]
    fn image_only_content_has_no_filler_text() {
        let content = json::parse(
            r#"[{"type":"image_url","image_url":{"url":"https://example.com/a.png"}}]"#,
        )
        .unwrap();
        let parts = chat_content_to_parts(Some(&content));
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].get_str("type"), Some("input_image"));
        assert_eq!(parts[0].get_str("detail"), Some("auto"));
    }

    #[test]
    fn string_content_is_single_text_part() {
        let content = Json::String("hello".to_string());
        let parts = chat_content_to_parts(Some(&content));
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].get_str("type"), Some("input_text"));
        assert_eq!(parts[0].get_str("text"), Some("hello"));
    }

    #[test]
    fn empty_content_falls_back_to_placeholder() {
        let parts = chat_content_to_parts(None);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].get_str("type"), Some("input_text"));
        assert_eq!(parts[0].get_str("text"), Some("..."));
    }

    #[test]
    fn models_json_uses_provider_prefixed_ids() {
        let cfg = test_config("prefixed-models");
        ensure_default_files(&cfg).unwrap();
        let parsed = json::parse(&models_json(&cfg)).unwrap();
        let ids = match parsed.get("data").unwrap() {
            Json::Array(items) => items
                .iter()
                .filter_map(|item| item.get_str("id"))
                .collect::<Vec<_>>(),
            _ => panic!("models data should be an array"),
        };
        assert!(ids.contains(&"codex/gpt-5.4-mini"));
        assert!(ids.contains(&"claude/claude-opus-4-8"));
        assert!(ids.contains(&"opencode-go/glm-5.2"));
        assert!(ids.contains(&"opencode-go/qwen3.7-plus"));
    }

    #[test]
    fn provider_prefixes_route_to_canonical_providers() {
        let cfg = test_config("provider-prefixes");
        ensure_default_files(&cfg).unwrap();
        assert_eq!(
            provider_for_model(&cfg, "codex/gpt-5.4-mini"),
            PROVIDER_CODEX
        );
        assert_eq!(
            provider_for_model(&cfg, "claude/claude-opus-4-8"),
            PROVIDER_CLAUDE
        );
        assert_eq!(
            provider_for_model(&cfg, "opencode-go/glm-5.2"),
            PROVIDER_OPENCODE_GO
        );
        assert_eq!(
            provider_for_model(&cfg, "ocg/qwen3.7-plus"),
            PROVIDER_OPENCODE_GO
        );
    }

    #[test]
    fn prefixed_models_normalize_for_upstream_requests() {
        let cfg = test_config("normalize-prefixes");
        ensure_default_files(&cfg).unwrap();
        let codex = transform_request(
            "/v1/chat/completions",
            br#"{"model":"codex/gpt-5.3-codex-high","messages":[{"role":"user","content":"hi"}]}"#,
            &cfg,
        )
        .unwrap();
        let parsed = json::parse(&codex).unwrap();
        assert_eq!(parsed.get_str("model"), Some("gpt-5.3-codex"));

        let opencode = transform_anthropic_messages_request(
            br#"{"model":"opencode-go/qwen3.7-plus","messages":[{"role":"user","content":"hi"}]}"#,
            &cfg,
            PROVIDER_OPENCODE_GO,
            DEFAULT_OPENCODE_SYSTEM_PROMPT,
        )
        .unwrap();
        let parsed = json::parse(&opencode).unwrap();
        assert_eq!(parsed.get_str("model"), Some("qwen3.7-plus"));
    }
}
