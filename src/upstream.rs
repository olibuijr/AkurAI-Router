use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use crate::config::{Config, load_models};
use crate::http;
use crate::json::{self, Json};
use crate::util::{base64_url_decode, now_secs};

const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are Codex, a focused software engineering agent. Answer accurately, preserve user intent, and use tools only when provided by the client.";

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

pub fn models_json(cfg: &Config) -> String {
    let mut data = Vec::new();
    for model in load_models(cfg).into_iter().filter(|m| m.enabled) {
        let mut obj = Json::object();
        obj.set("id", Json::String(model.id));
        obj.set("object", Json::String("model".to_string()));
        obj.set("owned_by", Json::String("codex".to_string()));
        data.push(obj);
    }
    let mut root = Json::object();
    root.set("object", Json::String("list".to_string()));
    root.set("data", Json::Array(data));
    root.stringify()
}

pub fn forward_codex(req: &http::Request, stream: &mut TcpStream, cfg: &Config) {
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

    if let Err(e) = curl_stream_post(&cfg.codex_responses_url, &headers, body.as_bytes(), stream) {
        let _ = http::send_json(stream, 502, &error_json(&e));
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
) -> Result<(), String> {
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
    Ok(())
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
        .trim_start_matches("cx/")
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
        let text = match msg.get("content") {
            Some(Json::String(s)) => s.clone(),
            Some(Json::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get_str("text"))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let mut item = Json::object();
        item.set("type", Json::String("message".to_string()));
        item.set("role", Json::String(role));
        let mut content = Json::object();
        content.set("type", Json::String("input_text".to_string()));
        content.set(
            "text",
            Json::String(if text.is_empty() {
                "...".to_string()
            } else {
                text
            }),
        );
        item.set("content", Json::Array(vec![content]));
        input.push(item);
    }
    out.set("input", Json::Array(input));
    Ok(out)
}

fn normalize_codex_body(body: &mut Json, cfg: &Config) -> Result<(), String> {
    let model = body
        .get_str("model")
        .unwrap_or(&cfg.default_model)
        .trim_start_matches("cx/")
        .to_string();
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
    let mut id = model.to_string();
    for effort in ["-none", "-low", "-medium", "-high", "-xhigh"] {
        if id.ends_with(effort) {
            id.truncate(id.len() - effort.len());
        }
    }
    for configured in load_models(cfg) {
        if configured.id == model {
            return configured.upstream_id;
        }
    }
    id
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
    let text = fs::read_to_string(&cfg.codex_auth_path).map_err(|e| {
        format!(
            "failed to read Codex auth at {}: {e}",
            cfg.codex_auth_path.display()
        )
    })?;
    json::parse(&text).map_err(|e| format!("failed to parse Codex auth JSON: {e}"))
}

fn write_codex_auth(cfg: &Config, root: &Json) -> Result<(), String> {
    fs::write(&cfg.codex_auth_path, root.stringify()).map_err(|e| e.to_string())?;
    let _ = fs::set_permissions(&cfg.codex_auth_path, fs::Permissions::from_mode(0o600));
    Ok(())
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
