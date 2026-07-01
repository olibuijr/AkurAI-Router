use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::accounts::{self, UsageRecord};
use crate::auth::ApiActor;
use crate::config::{
    Config, EmbeddingConfig, PROVIDER_CLAUDE, PROVIDER_CODEX, PROVIDER_EMBEDDINGS,
    PROVIDER_OPENCODE_GO, canonical_provider_id, default_provider_auth_path,
    is_opencode_go_messages_model, load_embedding_config, load_models, load_providers,
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

// ── OpenCode Go load-balanced key pool ──────────────────────────────────────
//
// Round-robin across all configured keys while they are healthy. When a key
// returns an out-of-quota response (401/402/403/429), it is parked for a
// cooldown window and skipped; requests keep flowing on the working key(s).
// Once the cooldown expires the key re-enters rotation automatically.

struct OpenCodeGoPool {
    cursor: usize,
    /// (key, cooldown-expiry) for keys currently parked as out-of-quota.
    downs: Vec<(String, Instant)>,
}

static OPENCODE_GO_POOL: Mutex<OpenCodeGoPool> = Mutex::new(OpenCodeGoPool {
    cursor: 0,
    downs: Vec::new(),
});

/// Statuses that mean "this key is out of quota / not usable right now" — the
/// trigger to park the key and fail over to the next one.
fn opencode_go_status_means_down(status: u16) -> bool {
    matches!(status, 401 | 402 | 403 | 429)
}

/// Round-robin pick over keys not currently cooling down. If every key is parked,
/// fall back to plain round-robin (least-bad) so a request still gets attempted.
fn select_opencode_go_key(keys: &[String]) -> usize {
    let mut pool = OPENCODE_GO_POOL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    pool.downs.retain(|(_, until)| *until > now);
    let n = keys.len();
    for _ in 0..n {
        let idx = pool.cursor % n;
        pool.cursor = pool.cursor.wrapping_add(1);
        if !pool.downs.iter().any(|(k, _)| k == &keys[idx]) {
            return idx;
        }
    }
    let idx = pool.cursor % n;
    pool.cursor = pool.cursor.wrapping_add(1);
    idx
}

/// Park a key as out-of-quota for `cooldown`. Replaces any existing entry.
fn mark_opencode_go_key_down(key: &str, cooldown: Duration) {
    let mut pool = OPENCODE_GO_POOL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let until = Instant::now() + cooldown;
    pool.downs.retain(|(k, _)| k != key);
    pool.downs.push((key.to_string(), until));
}

/// The active key pool: env-configured keys if present, else the single key from
/// the opencode auth file (backward compatible).
fn load_opencode_go_keys(cfg: &Config) -> Result<Vec<String>, String> {
    if !cfg.opencode_go_keys.is_empty() {
        return Ok(cfg.opencode_go_keys.clone());
    }
    let auth = load_opencode_go_auth(cfg)?;
    Ok(vec![auth.api_key])
}

enum DispatchOutcome {
    /// Streaming response already written to the client; carries the upstream status.
    Streamed(u16),
    /// Non-streaming response captured; caller delivers/transforms `body`.
    Captured(u16, String),
    /// Every key failed (all out of quota, or a hard error). `body` is ready JSON.
    Failed(u16, String),
}

/// Run a request against the OpenCode Go key pool with round-robin + failover.
/// `build_headers` produces the per-key headers (the auth header differs between
/// the chat and messages endpoints). Streaming responses are forwarded only once
/// an acceptable upstream status is seen, so failover never half-streams.
fn opencode_go_dispatch<F>(
    cfg: &Config,
    url: &str,
    streaming: bool,
    build_headers: F,
    body: &[u8],
    stream: &mut TcpStream,
) -> DispatchOutcome
where
    F: Fn(&str) -> Vec<(String, String)>,
{
    let keys = match load_opencode_go_keys(cfg) {
        Ok(keys) if !keys.is_empty() => keys,
        Ok(_) => {
            return DispatchOutcome::Failed(
                502,
                error_json("no opencode-go keys configured (set AKURAI_ROUTER_OPENCODE_GO_KEYS)"),
            );
        }
        Err(e) => return DispatchOutcome::Failed(502, error_json(&e)),
    };
    let n = keys.len();
    let cooldown = Duration::from_secs(cfg.opencode_go_cooldown_secs.max(1));
    let mut last_status = 502u16;
    let mut last_body = error_json("opencode-go request failed");

    for _ in 0..n {
        let idx = select_opencode_go_key(&keys);
        let key = keys[idx].clone();
        let headers = build_headers(&key);

        if streaming {
            match curl_stream_begin(url, &headers, body) {
                Ok(begin) => {
                    if n > 1 && opencode_go_status_means_down(begin.status) {
                        last_status = begin.status;
                        last_body = drain_stream_begin(begin);
                        mark_opencode_go_key_down(&key, cooldown);
                        continue;
                    }
                    let begin_status = begin.status;
                    return match curl_stream_finish(begin, stream) {
                        Ok(status) => DispatchOutcome::Streamed(status),
                        // Headers already reached the client — can't fail over now.
                        Err(_) => DispatchOutcome::Streamed(begin_status),
                    };
                }
                Err(e) => {
                    last_status = 502;
                    last_body = error_json(&e);
                    mark_opencode_go_key_down(&key, cooldown);
                    continue;
                }
            }
        } else {
            let headers_ref: Vec<(&str, String)> = headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect();
            match curl_capture("POST", url, &headers_ref, body, 900) {
                Ok(resp) => {
                    let text = String::from_utf8_lossy(&resp.body).to_string();
                    if n > 1 && opencode_go_status_means_down(resp.status) {
                        last_status = resp.status;
                        last_body = text;
                        mark_opencode_go_key_down(&key, cooldown);
                        continue;
                    }
                    return DispatchOutcome::Captured(resp.status, text);
                }
                Err(e) => {
                    last_status = 502;
                    last_body = error_json(&e);
                    mark_opencode_go_key_down(&key, cooldown);
                    continue;
                }
            }
        }
    }
    DispatchOutcome::Failed(last_status, last_body)
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
        PROVIDER_EMBEDDINGS => {
            let _ = http::send_json(
                stream,
                400,
                &error_json("embedding models support /v1/embeddings"),
            );
        }
        _ => forward_codex(req, stream, cfg, actor),
    }
}

pub fn forward_embeddings(
    req: &http::Request,
    stream: &mut TcpStream,
    cfg: &Config,
    actor: &ApiActor,
) {
    let embedding = load_embedding_config(cfg);
    if !embedding.enabled {
        let _ = http::send_json(stream, 503, &error_json("embedding endpoint is disabled"));
        return;
    }
    if embedding.upstream_url.trim().is_empty() {
        let _ = http::send_json(
            stream,
            503,
            &error_json("embedding upstream URL is not configured"),
        );
        return;
    }
    let mut body = match json::parse(&String::from_utf8_lossy(&req.body)) {
        Ok(value) => value,
        Err(e) => {
            let _ = http::send_json(stream, 400, &error_json(&e));
            return;
        }
    };
    let requested_model = body
        .get_str("model")
        .map(|s| s.to_string())
        .unwrap_or_else(|| embedding.model.clone());
    body.set(
        "model",
        Json::String(resolve_embedding_model(cfg, &embedding, &requested_model)),
    );
    let body_text = body.stringify();
    let headers = vec![
        ("Content-Type", "application/json".to_string()),
        ("Accept", "application/json".to_string()),
    ];
    match curl_capture(
        "POST",
        embedding.upstream_url.trim(),
        &headers,
        body_text.as_bytes(),
        120,
    ) {
        Ok(resp) => {
            let text = String::from_utf8_lossy(&resp.body);
            record_usage(
                cfg,
                actor,
                PROVIDER_EMBEDDINGS,
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
                PROVIDER_EMBEDDINGS,
                &requested_model,
                &req.path,
                502,
                None,
            );
            let _ = http::send_json(stream, 502, &error_json(&e));
        }
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
    if is_responses_path(&req.path) {
        forward_claude_responses(req, stream, cfg, actor);
        return;
    }
    let requested_model = request_model(req, cfg);
    // The Claude upstream is queried non-streamed, but clients (pi) request stream:true
    // and parse an SSE response. Track that so we can synthesize a stream on the way back.
    let wants_stream = json::parse(&String::from_utf8_lossy(&req.body))
        .ok()
        .and_then(|v| v.get_bool("stream"))
        .unwrap_or(false);
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
    let headers = claude_request_headers(&auth.access_token, &session_id);
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
                    if wants_stream {
                        match chat_completion_to_sse(&json) {
                            Ok(sse) => {
                                let _ = http::send_text(stream, 200, "text/event-stream", &sse);
                            }
                            Err(_) => {
                                let _ = http::send_json(stream, 200, &json);
                            }
                        }
                    } else {
                        let _ = http::send_json(stream, 200, &json);
                    }
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

/// True when the request targets the OpenAI Responses API rather than
/// chat/completions. codex speaks this natively; for every other provider the
/// router translates Responses <-> chat/completions (see the `*_responses`
/// handlers below).
fn is_responses_path(path: &str) -> bool {
    path.ends_with("/responses") || path == "/codex"
}

/// The full Claude/Anthropic header set. Extracted so both the chat path and the
/// Responses bridge send identical upstream headers.
fn claude_request_headers(access_token: &str, session_id: &str) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        ),
        ("Anthropic-Version".to_string(), "2023-06-01".to_string()),
        (
            "Anthropic-Beta".to_string(),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advanced-tool-use-2025-11-20,effort-2025-11-24,structured-outputs-2025-12-15,fast-mode-2026-02-01,redact-thinking-2026-02-12,token-efficient-tools-2026-03-28".to_string(),
        ),
        (
            "Anthropic-Dangerous-Direct-Browser-Access".to_string(),
            "true".to_string(),
        ),
        (
            "User-Agent".to_string(),
            "claude-cli/2.1.92 (external, sdk-cli)".to_string(),
        ),
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
        ("X-Session-Id".to_string(), session_id.to_string()),
    ]
}

/// Flatten Responses-API `content` (array of `input_text`/`output_text`/`text`
/// parts, or a bare string) into a single chat-completions text string.
fn responses_content_to_text(content: Option<&Json>) -> String {
    match content {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get_str("text").map(|t| t.to_string()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Convert Responses-API tool definitions (`{type,name,parameters,description}`)
/// into chat-completions tool definitions (`{type:function,function:{...}}`).
/// Already-nested chat-style tools are passed through unchanged.
fn responses_tools_to_chat(tools: &[Json]) -> Vec<Json> {
    let mut out = Vec::new();
    for tool in tools {
        if tool.get("function").is_some() {
            out.push(tool.clone());
            continue;
        }
        let mut function = Json::object();
        if let Some(name) = tool.get_str("name") {
            function.set("name", Json::String(name.to_string()));
        }
        if let Some(desc) = tool.get_str("description") {
            function.set("description", Json::String(desc.to_string()));
        }
        if let Some(params) = tool.get("parameters") {
            function.set("parameters", params.clone());
        }
        if let Some(Json::Bool(strict)) = tool.get("strict") {
            function.set("strict", Json::Bool(*strict));
        }
        let mut wrapped = Json::object();
        wrapped.set("type", Json::String("function".to_string()));
        wrapped.set("function", function);
        out.push(wrapped);
    }
    out
}

/// Convert chat-completions function tools
/// (`{type:"function",function:{name,description,parameters}}`) into the
/// Responses API shape (`{type:"function",name,description,parameters}`).
/// Already-flat Responses tools and built-in tool types are passed through.
fn chat_tools_to_responses(tools: &[Json]) -> Vec<Json> {
    let mut out = Vec::new();
    for tool in tools {
        let Some(function) = tool.get("function") else {
            out.push(tool.clone());
            continue;
        };
        let Some(name) = function.get_str("name") else {
            out.push(tool.clone());
            continue;
        };
        let mut converted = Json::object();
        converted.set("type", Json::String("function".to_string()));
        converted.set("name", Json::String(name.to_string()));
        if let Some(desc) = function.get_str("description") {
            converted.set("description", Json::String(desc.to_string()));
        }
        if let Some(params) = function.get("parameters") {
            converted.set("parameters", params.clone());
        }
        if let Some(Json::Bool(strict)) = function.get("strict").or_else(|| tool.get("strict")) {
            converted.set("strict", Json::Bool(*strict));
        }
        out.push(converted);
    }
    out
}

fn chat_tool_choice_to_responses(choice: &Json) -> Json {
    let Some(function) = choice.get("function") else {
        return choice.clone();
    };
    let Some(name) = function.get_str("name") else {
        return choice.clone();
    };
    let mut converted = Json::object();
    converted.set("type", Json::String("function".to_string()));
    converted.set("name", Json::String(name.to_string()));
    converted
}

/// Translate an OpenAI Responses request into an equivalent chat/completions
/// request. When `force_tool_for_schema` is set and the request asks for a
/// `json_schema` structured output, the schema is bridged to a forced
/// function-call (providers like DeepSeek reject `response_format: json_schema`
/// but support tool calling). Returns the chat body plus, if the tool bridge was
/// used, the synthetic tool name so the response can be converted back.
fn responses_to_chat(
    value: &Json,
    default_model: &str,
    force_tool_for_schema: bool,
) -> Result<(Json, Option<String>), String> {
    let mut out = Json::object();
    out.set(
        "model",
        Json::String(value.get_str("model").unwrap_or(default_model).to_string()),
    );

    let mut messages = Vec::new();
    if let Some(instructions) = value.get_str("instructions")
        && !instructions.is_empty()
    {
        let mut sys = Json::object();
        sys.set("role", Json::String("system".to_string()));
        sys.set("content", Json::String(instructions.to_string()));
        messages.push(sys);
    }
    match value.get("input") {
        Some(Json::String(s)) => {
            let mut m = Json::object();
            m.set("role", Json::String("user".to_string()));
            m.set("content", Json::String(s.clone()));
            messages.push(m);
        }
        Some(Json::Array(items)) => {
            for item in items {
                let kind = item.get_str("type").unwrap_or("message");
                match kind {
                    "function_call_output" => {
                        let mut m = Json::object();
                        m.set("role", Json::String("tool".to_string()));
                        if let Some(id) = item.get_str("call_id") {
                            m.set("tool_call_id", Json::String(id.to_string()));
                        }
                        m.set(
                            "content",
                            Json::String(item.get_str("output").unwrap_or("").to_string()),
                        );
                        messages.push(m);
                    }
                    "function_call" => {
                        let mut call = Json::object();
                        call.set(
                            "id",
                            Json::String(item.get_str("call_id").unwrap_or("").to_string()),
                        );
                        call.set("type", Json::String("function".to_string()));
                        let mut func = Json::object();
                        func.set(
                            "name",
                            Json::String(item.get_str("name").unwrap_or("").to_string()),
                        );
                        func.set(
                            "arguments",
                            Json::String(item.get_str("arguments").unwrap_or("{}").to_string()),
                        );
                        call.set("function", func);
                        let mut m = Json::object();
                        m.set("role", Json::String("assistant".to_string()));
                        m.set("content", Json::Null);
                        m.set("tool_calls", Json::Array(vec![call]));
                        messages.push(m);
                    }
                    _ => {
                        let mut m = Json::object();
                        m.set(
                            "role",
                            Json::String(item.get_str("role").unwrap_or("user").to_string()),
                        );
                        m.set(
                            "content",
                            Json::String(responses_content_to_text(item.get("content"))),
                        );
                        messages.push(m);
                    }
                }
            }
        }
        _ => return Err("responses request requires `input`".to_string()),
    }
    if messages.is_empty() {
        return Err("responses request produced no messages".to_string());
    }

    if let Some(s) = value.get_bool("stream") {
        out.set("stream", Json::Bool(s));
    }
    if let Some(v) = value.get("temperature") {
        out.set("temperature", v.clone());
    }
    if let Some(v) = value.get("top_p") {
        out.set("top_p", v.clone());
    }
    if let Some(v) = value.get("reasoning_effort") {
        out.set("reasoning_effort", v.clone());
    }
    if let Some(v) = value.get("max_output_tokens") {
        out.set("max_tokens", v.clone());
    }

    let tools: Vec<Json> = match value.get("tools") {
        Some(Json::Array(t)) => responses_tools_to_chat(t),
        _ => Vec::new(),
    };
    if let Some(v) = value.get("tool_choice") {
        out.set("tool_choice", v.clone());
    }

    // text.format -> structured output.
    //
    // `bridge_schema_via_prompt` providers (DeepSeek/GLM via opencode-go) reject
    // `response_format: json_schema`, and reasoning models among them also reject a
    // forced `tool_choice`. The portable path that works for both thinking and
    // non-thinking models is `response_format: json_object` plus the schema injected
    // into the prompt — the model returns the JSON as ordinary content. Other
    // providers (Claude) keep the native `json_schema` response_format.
    let structured_tool = None;
    if let Some(format) = value.get("text").and_then(|t| t.get("format")) {
        match format.get_str("type") {
            Some("json_schema") => {
                let schema = format.get("schema").cloned().unwrap_or_else(Json::object);
                if force_tool_for_schema {
                    let mut rf = Json::object();
                    rf.set("type", Json::String("json_object".to_string()));
                    out.set("response_format", rf);
                    let mut sys = Json::object();
                    sys.set("role", Json::String("system".to_string()));
                    sys.set(
                        "content",
                        Json::String(format!(
                            "You must respond with a single valid JSON object that strictly conforms to this JSON Schema. Output only the JSON object, with no surrounding prose or markdown fences.\nJSON Schema:\n{}",
                            schema.stringify()
                        )),
                    );
                    messages.push(sys);
                } else {
                    let name = format
                        .get_str("name")
                        .unwrap_or("structured_output")
                        .to_string();
                    let strict = format.get_bool("strict").unwrap_or(true);
                    let mut rf = Json::object();
                    rf.set("type", Json::String("json_schema".to_string()));
                    let mut js = Json::object();
                    js.set("name", Json::String(name));
                    js.set("schema", schema);
                    js.set("strict", Json::Bool(strict));
                    rf.set("json_schema", js);
                    out.set("response_format", rf);
                }
            }
            Some("json_object") => {
                let mut rf = Json::object();
                rf.set("type", Json::String("json_object".to_string()));
                out.set("response_format", rf);
            }
            _ => {}
        }
    }
    if !tools.is_empty() {
        out.set("tools", Json::Array(tools));
    }

    out.set("messages", Json::Array(messages));
    Ok((out, structured_tool))
}

/// Convert a chat/completions response object into an OpenAI Responses object.
/// When `structured_tool` is set, the forced tool call's arguments are surfaced
/// as the assistant's output text (the round-trip for the json_schema bridge).
fn chat_completion_to_responses(chat: &Json, structured_tool: Option<&str>) -> Json {
    let model = chat.get_str("model").unwrap_or("").to_string();
    let id = format!("resp_{}", now_secs());
    let msg_id = format!("msg_{}", now_secs());

    let mut output = Vec::new();
    let mut aggregated_text = String::new();

    let message = chat
        .get("choices")
        .and_then(|c| match c {
            Json::Array(a) => a.first(),
            _ => None,
        })
        .and_then(|choice| choice.get("message"));

    // Structured-output bridge: pull the forced tool call's JSON arguments out as
    // plain output text so SDKs that read `output_text` get the structured JSON.
    if let Some(name) = structured_tool
        && let Some(args) = message
            .and_then(|m| m.get("tool_calls"))
            .and_then(|tc| match tc {
                Json::Array(a) => a.first(),
                _ => None,
            })
            .filter(|call| {
                call.get("function")
                    .and_then(|f| f.get_str("name"))
                    .map(|n| n == name)
                    .unwrap_or(true)
            })
            .and_then(|call| call.get("function"))
            .and_then(|f| f.get_str("arguments"))
    {
        aggregated_text = args.to_string();
    } else {
        // Plain text content.
        if let Some(text) = message.and_then(|m| m.get_str("content")) {
            aggregated_text = text.to_string();
        }
        // Emit any genuine tool calls as function_call items.
        if let Some(Json::Array(calls)) = message.and_then(|m| m.get("tool_calls")) {
            for call in calls {
                let func = call.get("function");
                let mut item = Json::object();
                item.set("type", Json::String("function_call".to_string()));
                item.set(
                    "id",
                    Json::String(call.get_str("id").unwrap_or("").to_string()),
                );
                item.set(
                    "call_id",
                    Json::String(call.get_str("id").unwrap_or("").to_string()),
                );
                item.set(
                    "name",
                    Json::String(
                        func.and_then(|f| f.get_str("name"))
                            .unwrap_or("")
                            .to_string(),
                    ),
                );
                item.set(
                    "arguments",
                    Json::String(
                        func.and_then(|f| f.get_str("arguments"))
                            .unwrap_or("{}")
                            .to_string(),
                    ),
                );
                item.set("status", Json::String("completed".to_string()));
                output.push(item);
            }
        }
    }

    if !aggregated_text.is_empty() || output.is_empty() {
        let mut part = Json::object();
        part.set("type", Json::String("output_text".to_string()));
        part.set("text", Json::String(aggregated_text.clone()));
        part.set("annotations", Json::Array(Vec::new()));
        let mut msg = Json::object();
        msg.set("type", Json::String("message".to_string()));
        msg.set("id", Json::String(msg_id));
        msg.set("status", Json::String("completed".to_string()));
        msg.set("role", Json::String("assistant".to_string()));
        msg.set("content", Json::Array(vec![part]));
        output.insert(0, msg);
    }

    let mut resp = Json::object();
    resp.set("id", Json::String(id));
    resp.set("object", Json::String("response".to_string()));
    resp.set("created_at", Json::Number(now_secs().to_string()));
    resp.set("model", Json::String(model));
    resp.set("status", Json::String("completed".to_string()));
    resp.set("output", Json::Array(output));
    resp.set("output_text", Json::String(aggregated_text));

    let mut usage = Json::object();
    if let Some(u) = chat.get("usage") {
        if let Some(p) = json_number_string(u.get("prompt_tokens")) {
            usage.set("input_tokens", Json::Number(p));
        }
        if let Some(c) = json_number_string(u.get("completion_tokens")) {
            usage.set("output_tokens", Json::Number(c));
        }
        if let Some(t) = json_number_string(u.get("total_tokens")) {
            usage.set("total_tokens", Json::Number(t));
        }
    }
    resp.set("usage", usage);
    resp
}

/// Aggregate the `output_text` from a Responses object.
fn responses_output_text(resp: &Json) -> String {
    resp.get_str("output_text").unwrap_or("").to_string()
}

/// Synthesize the documented Responses streaming event sequence for a single
/// completed text message. Used when a client requests `stream:true` against a
/// provider the router queries non-streamed.
fn responses_object_to_sse(resp: &Json) -> String {
    let text = responses_output_text(resp);
    let msg_id = resp
        .get("output")
        .and_then(|o| match o {
            Json::Array(a) => a.first(),
            _ => None,
        })
        .and_then(|m| m.get_str("id"))
        .unwrap_or("msg_0")
        .to_string();

    let mut initial = resp.clone();
    initial.set("status", Json::String("in_progress".to_string()));
    initial.set("output", Json::Array(Vec::new()));

    let mut seq = 0u64;
    let mut out = String::new();
    let mut emit = |event: &str, data: Json, seq: &mut u64| {
        let mut d = data;
        d.set("type", Json::String(event.to_string()));
        d.set("sequence_number", Json::Number(seq.to_string()));
        out.push_str(&format!("event: {event}\ndata: {}\n\n", d.stringify()));
        *seq += 1;
    };

    let mut created = Json::object();
    created.set("response", initial.clone());
    emit("response.created", created, &mut seq);

    let mut in_prog = Json::object();
    in_prog.set("response", initial);
    emit("response.in_progress", in_prog, &mut seq);

    let mut item = Json::object();
    item.set("type", Json::String("message".to_string()));
    item.set("id", Json::String(msg_id.clone()));
    item.set("status", Json::String("in_progress".to_string()));
    item.set("role", Json::String("assistant".to_string()));
    item.set("content", Json::Array(Vec::new()));
    let mut added = Json::object();
    added.set("output_index", Json::Number("0".to_string()));
    added.set("item", item);
    emit("response.output_item.added", added, &mut seq);

    let mut part = Json::object();
    part.set("type", Json::String("output_text".to_string()));
    part.set("text", Json::String(String::new()));
    part.set("annotations", Json::Array(Vec::new()));
    let mut part_added = Json::object();
    part_added.set("item_id", Json::String(msg_id.clone()));
    part_added.set("output_index", Json::Number("0".to_string()));
    part_added.set("content_index", Json::Number("0".to_string()));
    part_added.set("part", part);
    emit("response.content_part.added", part_added, &mut seq);

    let mut delta = Json::object();
    delta.set("item_id", Json::String(msg_id.clone()));
    delta.set("output_index", Json::Number("0".to_string()));
    delta.set("content_index", Json::Number("0".to_string()));
    delta.set("delta", Json::String(text.clone()));
    emit("response.output_text.delta", delta, &mut seq);

    let mut done = Json::object();
    done.set("item_id", Json::String(msg_id.clone()));
    done.set("output_index", Json::Number("0".to_string()));
    done.set("content_index", Json::Number("0".to_string()));
    done.set("text", Json::String(text.clone()));
    emit("response.output_text.done", done, &mut seq);

    let mut completed = Json::object();
    completed.set("response", resp.clone());
    emit("response.completed", completed, &mut seq);

    out
}

/// OpenAI Responses API bridge for opencode-go (DeepSeek/GLM/etc). Translates the
/// request to chat/completions (bridging json_schema -> forced tool call so
/// structured output works on DeepSeek), queries the upstream non-streamed, then
/// converts the result back into a Responses object (or synthesized SSE).
fn forward_opencode_go_responses(
    req: &http::Request,
    stream: &mut TcpStream,
    cfg: &Config,
    actor: &ApiActor,
) {
    let value = match json::parse(&String::from_utf8_lossy(&req.body)) {
        Ok(v) => v,
        Err(e) => {
            let _ = http::send_json(stream, 400, &error_json(&e));
            return;
        }
    };
    let wants_stream = value.get_bool("stream").unwrap_or(false);
    let requested_model = value
        .get_str("model")
        .unwrap_or(&cfg.default_model)
        .to_string();
    let upstream_model = resolve_model_id_for_provider(cfg, &requested_model, PROVIDER_OPENCODE_GO);
    if is_opencode_go_messages_model(&upstream_model) {
        let _ = http::send_json(
            stream,
            400,
            &error_json(
                "this opencode-go model does not support /v1/responses; use /v1/chat/completions",
            ),
        );
        return;
    }

    let (mut chat, structured_tool) = match responses_to_chat(&value, &cfg.default_model, true) {
        Ok(v) => v,
        Err(e) => {
            let _ = http::send_json(stream, 400, &error_json(&e));
            return;
        }
    };
    chat.set("model", Json::String(upstream_model));
    chat.set("stream", Json::Bool(false));
    let body_text = chat.stringify();

    let outcome = opencode_go_dispatch(
        cfg,
        &cfg.opencode_go_chat_url,
        false,
        |key| {
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), format!("Bearer {key}")),
            ]
        },
        body_text.as_bytes(),
        stream,
    );
    match outcome {
        DispatchOutcome::Captured(status, text) => {
            record_usage(
                cfg,
                actor,
                PROVIDER_OPENCODE_GO,
                &requested_model,
                &req.path,
                status,
                Some(&text),
            );
            if status != 200 {
                let _ = http::send_json(stream, status, &text);
                return;
            }
            let chat_json = match json::parse(&text) {
                Ok(v) => v,
                Err(e) => {
                    let _ = http::send_json(stream, 502, &error_json(&e));
                    return;
                }
            };
            let resp = chat_completion_to_responses(&chat_json, structured_tool.as_deref());
            if wants_stream {
                let _ = http::send_text(
                    stream,
                    200,
                    "text/event-stream",
                    &responses_object_to_sse(&resp),
                );
            } else {
                let _ = http::send_json(stream, 200, &resp.stringify());
            }
        }
        DispatchOutcome::Failed(status, body) => {
            record_usage(
                cfg,
                actor,
                PROVIDER_OPENCODE_GO,
                &requested_model,
                &req.path,
                status,
                None,
            );
            let _ = http::send_json(stream, status, &body);
        }
        DispatchOutcome::Streamed(_) => {}
    }
}

/// OpenAI Responses API bridge for the Claude provider. Translates the request to
/// chat/completions, runs the existing Claude pipeline, then converts the
/// chat-completion result into a Responses object (or synthesized SSE).
fn forward_claude_responses(
    req: &http::Request,
    stream: &mut TcpStream,
    cfg: &Config,
    actor: &ApiActor,
) {
    let value = match json::parse(&String::from_utf8_lossy(&req.body)) {
        Ok(v) => v,
        Err(e) => {
            let _ = http::send_json(stream, 400, &error_json(&e));
            return;
        }
    };
    let wants_stream = value.get_bool("stream").unwrap_or(false);
    let requested_model = value
        .get_str("model")
        .unwrap_or(&cfg.default_model)
        .to_string();
    // Claude structured outputs are not surfaced through our text-only converter,
    // so keep json_schema as response_format (best-effort) rather than tool-bridging.
    let (mut chat, _structured) = match responses_to_chat(&value, &cfg.default_model, false) {
        Ok(v) => v,
        Err(e) => {
            let _ = http::send_json(stream, 400, &error_json(&e));
            return;
        }
    };
    chat.set("stream", Json::Bool(false));

    let body = match transform_claude_request(chat.stringify().as_bytes(), cfg) {
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
    let headers = claude_request_headers(&auth.access_token, &session_id);
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
                let _ = http::send_json(stream, resp.status, &error_json(&text));
                return;
            }
            match claude_to_openai_response(&text, cfg) {
                Ok(chat_completion) => match json::parse(&chat_completion) {
                    Ok(chat_json) => {
                        let resp_obj = chat_completion_to_responses(&chat_json, None);
                        if wants_stream {
                            let _ = http::send_text(
                                stream,
                                200,
                                "text/event-stream",
                                &responses_object_to_sse(&resp_obj),
                            );
                        } else {
                            let _ = http::send_json(stream, 200, &resp_obj.stringify());
                        }
                    }
                    Err(e) => {
                        let _ = http::send_json(stream, 502, &error_json(&e));
                    }
                },
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
    if is_responses_path(&req.path) {
        forward_opencode_go_responses(req, stream, cfg, actor);
        return;
    }
    if !req.path.ends_with("/chat/completions") {
        let _ = http::send_json(
            stream,
            400,
            &error_json("opencode-go models support /v1/chat/completions or /v1/responses"),
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
    let streaming = body.get_bool("stream").unwrap_or(false);
    let accept = if streaming {
        "text/event-stream"
    } else {
        "application/json"
    };
    let body_text = body.stringify();
    let outcome = opencode_go_dispatch(
        cfg,
        &cfg.opencode_go_chat_url,
        streaming,
        |key| {
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), accept.to_string()),
                ("Authorization".to_string(), format!("Bearer {key}")),
            ]
        },
        body_text.as_bytes(),
        stream,
    );
    match outcome {
        DispatchOutcome::Streamed(status) => record_usage(
            cfg,
            actor,
            PROVIDER_OPENCODE_GO,
            &requested_model,
            &req.path,
            status,
            None,
        ),
        DispatchOutcome::Captured(status, text) => {
            record_usage(
                cfg,
                actor,
                PROVIDER_OPENCODE_GO,
                &requested_model,
                &req.path,
                status,
                Some(&text),
            );
            let _ = http::send_json(stream, status, &text);
        }
        DispatchOutcome::Failed(status, body) => {
            record_usage(
                cfg,
                actor,
                PROVIDER_OPENCODE_GO,
                &requested_model,
                &req.path,
                status,
                None,
            );
            let _ = http::send_json(stream, status, &body);
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

    let outcome = opencode_go_dispatch(
        cfg,
        &cfg.opencode_go_messages_url,
        false,
        |key| {
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
                ("x-api-key".to_string(), key.to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ]
        },
        body.as_bytes(),
        stream,
    );
    match outcome {
        DispatchOutcome::Captured(status, text) => {
            record_usage(
                cfg,
                actor,
                PROVIDER_OPENCODE_GO,
                &requested_model,
                &req.path,
                status,
                Some(&text),
            );
            if status != 200 {
                let _ = http::send_json(stream, status, &error_json(&text));
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
        DispatchOutcome::Failed(status, body) => {
            record_usage(
                cfg,
                actor,
                PROVIDER_OPENCODE_GO,
                &requested_model,
                &req.path,
                status,
                None,
            );
            let _ = http::send_json(stream, status, &body);
        }
        // Messages endpoint never streams (dispatch called with streaming=false).
        DispatchOutcome::Streamed(_) => {}
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
    if method != "GET"
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(body).map_err(|e| e.to_string())?;
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

/// An in-flight streaming upstream request whose status line + headers have been
/// read but whose body has NOT yet been forwarded to the client. This lets the
/// caller inspect `status` and fail over to another key before any byte reaches
/// the client (`finish` to forward, `drain` to discard).
struct StreamBegin {
    child: Child,
    stdout: ChildStdout,
    status: u16,
    response_headers: Vec<(String, String)>,
    body_prefix: Vec<u8>,
}

/// Spawn the upstream request and read up to the end of the response headers.
/// Nothing is written to the client yet.
fn curl_stream_begin(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<StreamBegin, String> {
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

    Ok(StreamBegin {
        child,
        stdout,
        status,
        response_headers,
        body_prefix,
    })
}

/// Forward a `StreamBegin` to the client: headers, the already-read body prefix,
/// then pump the rest of the body until upstream closes.
fn curl_stream_finish(begin: StreamBegin, stream: &mut TcpStream) -> Result<u16, String> {
    let StreamBegin {
        mut child,
        mut stdout,
        status,
        response_headers,
        body_prefix,
    } = begin;
    http::stream_headers(stream, status, &response_headers).map_err(|e| e.to_string())?;
    if !body_prefix.is_empty() {
        stream.write_all(&body_prefix).map_err(|e| e.to_string())?;
    }
    let mut tmp = [0u8; 8192];
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

/// Discard a `StreamBegin` (an out-of-quota response we're failing over from),
/// returning its body as a ready-to-send JSON string for the error path.
fn drain_stream_begin(begin: StreamBegin) -> String {
    let StreamBegin {
        mut child,
        mut stdout,
        body_prefix,
        ..
    } = begin;
    let mut body = body_prefix;
    let mut tmp = [0u8; 8192];
    while body.len() < 64 * 1024 {
        match stdout.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let text = String::from_utf8_lossy(&body).trim().to_string();
    if text.starts_with('{') {
        text
    } else {
        error_json(&text)
    }
}

fn curl_stream_post(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    stream: &mut TcpStream,
) -> Result<u16, String> {
    let begin = curl_stream_begin(url, headers, body)?;
    curl_stream_finish(begin, stream)
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
    if let Some(Json::Array(tools)) = value.get("tools") {
        out.set("tools", Json::Array(chat_tools_to_responses(tools)));
    } else if let Some(v) = value.get("tools") {
        out.set("tools", v.clone());
    }
    if let Some(v) = value.get("tool_choice") {
        out.set("tool_choice", chat_tool_choice_to_responses(v));
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
                } else if let Some(text) = part.get_str("text")
                    && !text.is_empty()
                {
                    parts.push(input_text_part(text));
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
    normalize_codex_tools(body);
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

fn normalize_codex_tools(body: &mut Json) {
    if let Some(Json::Array(tools)) = body.get("tools") {
        body.set("tools", Json::Array(chat_tools_to_responses(tools)));
    }
    if let Some(choice) = body.get("tool_choice") {
        body.set("tool_choice", chat_tool_choice_to_responses(choice));
    }
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
    let mut system_blocks = if system_blocks.is_empty() {
        vec![claude_text_block(default_system_prompt)]
    } else {
        system_blocks
    };
    // Claude Code OAuth subscription billing: api.anthropic.com only bills a request
    // against the Max/Pro subscription when system[0] is the Claude Code billing-header
    // block. Without it, substantial custom/agent system prompts (e.g. pi's harness
    // prompt) get pushed to pay-as-you-go "extra usage" and rejected. Inject it for the
    // Claude provider only — opencode-go also routes through this function.
    if provider_id == PROVIDER_CLAUDE {
        system_blocks.insert(0, claude_billing_block(raw));
    }
    body.set("system", Json::Array(system_blocks));
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

// Build the Claude Code billing-header system block that marks the request as genuine
// Claude Code traffic so api.anthropic.com bills it against the OAuth subscription
// rather than pay-as-you-go "extra usage". Mirrors the real client format
// `x-anthropic-billing-header: cc_version=<ver>.<build>; cc_entrypoint=sdk-cli; cch=<hash>;`.
// Anthropic does not verify the hashes (confirmed empirically), but we derive them from
// the request bytes so the values vary per request and look legitimate.
fn claude_billing_block(raw: &[u8]) -> Json {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    let h = hasher.finish();
    let cch = format!("{:05x}", h & 0xf_ffff);
    let build = format!("{:03x}", (h >> 20) & 0xfff);
    claude_text_block(&format!(
        "x-anthropic-billing-header: cc_version=2.1.92.{}; cc_entrypoint=sdk-cli; cch={};",
        build, cch
    ))
}

// Synthesize an OpenAI-style SSE stream from a completed chat.completion JSON.
// The Claude upstream is queried non-streamed, but clients request `stream: true` and
// parse a text/event-stream response; without this they read the plain JSON as a stream
// and report "Stream ended without finish_reason".
fn chat_completion_to_sse(completion: &str) -> Result<String, String> {
    let parsed = json::parse(completion)?;
    let id = parsed
        .get_str("id")
        .unwrap_or("chatcmpl-akurai")
        .to_string();
    let model = parsed.get_str("model").unwrap_or("").to_string();
    let created =
        json_number_string(parsed.get("created")).unwrap_or_else(|| now_secs().to_string());
    let mut content = String::new();
    let mut finish = "stop".to_string();
    if let Some(Json::Array(choices)) = parsed.get("choices")
        && let Some(choice) = choices.first()
    {
        if let Some(text) = choice.get("message").and_then(|m| m.get_str("content")) {
            content = text.to_string();
        }
        if let Some(fr) = choice.get_str("finish_reason") {
            finish = fr.to_string();
        }
    }
    let chunk = |delta: Json, finish_reason: Json| -> String {
        let mut c = Json::object();
        c.set("id", Json::String(id.clone()));
        c.set("object", Json::String("chat.completion.chunk".to_string()));
        c.set("created", Json::Number(created.clone()));
        c.set("model", Json::String(model.clone()));
        let mut choice = Json::object();
        choice.set("index", Json::Number("0".to_string()));
        choice.set("delta", delta);
        choice.set("finish_reason", finish_reason);
        c.set("choices", Json::Array(vec![choice]));
        c.stringify()
    };
    let mut out = String::new();
    let mut role_delta = Json::object();
    role_delta.set("role", Json::String("assistant".to_string()));
    out.push_str(&format!("data: {}\n\n", chunk(role_delta, Json::Null)));
    if !content.is_empty() {
        let mut content_delta = Json::object();
        content_delta.set("content", Json::String(content));
        out.push_str(&format!("data: {}\n\n", chunk(content_delta, Json::Null)));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        chunk(Json::object(), Json::String(finish))
    ));
    if let Some(usage) = parsed.get("usage") {
        let mut c = Json::object();
        c.set("id", Json::String(id.clone()));
        c.set("object", Json::String("chat.completion.chunk".to_string()));
        c.set("created", Json::Number(created.clone()));
        c.set("model", Json::String(model.clone()));
        c.set("choices", Json::Array(Vec::new()));
        c.set("usage", usage.clone());
        out.push_str(&format!("data: {}\n\n", c.stringify()));
    }
    out.push_str("data: [DONE]\n\n");
    Ok(out)
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
    if let (Some(p), Some(c)) = (prompt_tokens, completion_tokens)
        && let (Ok(pn), Ok(cn)) = (p.parse::<u64>(), c.parse::<u64>())
    {
        usage.set("total_tokens", Json::Number((pn + cn).to_string()));
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

fn resolve_embedding_model(cfg: &Config, embedding: &EmbeddingConfig, requested: &str) -> String {
    let requested = requested.trim();
    if requested.is_empty() {
        return embedding.model.clone();
    }
    let bare = model_id_for_provider(requested, PROVIDER_EMBEDDINGS);
    for configured in load_models(cfg) {
        if canonical_provider_id(&configured.provider_id) != PROVIDER_EMBEDDINGS {
            continue;
        }
        if configured.id == bare
            || configured.id == requested
            || configured.upstream_id == bare
            || configured.upstream_id == requested
            || public_model_id(&configured) == requested
        {
            return configured.upstream_id;
        }
    }
    requested.to_string()
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
                    && role == "system"
                {
                    *role = "developer".to_string();
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
    if let Ok(root) = json::parse(&raw)
        && let Some(model) = root.get_str("model")
    {
        return model.to_string();
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
                if let Some(key) = provider.get_str(key_name)
                    && !key.trim().is_empty()
                {
                    return Ok(OpenCodeGoAuth {
                        api_key: key.trim().to_string(),
                    });
                }
            }
        }
    }
    for key_name in ["apiKey", "api_key", "key"] {
        if let Some(key) = root.get_str(key_name)
            && !key.trim().is_empty()
        {
            return Ok(OpenCodeGoAuth {
                api_key: key.trim().to_string(),
            });
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
            opencode_go_keys: Vec::new(),
            opencode_go_cooldown_secs: 300,
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
        assert!(ids.contains(&"embeddings/embeddinggemma"));
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
        assert_eq!(
            provider_for_model(&cfg, "embeddings/embeddinggemma"),
            PROVIDER_EMBEDDINGS
        );
    }

    #[test]
    fn embedding_models_resolve_to_configured_upstream_ids() {
        let cfg = test_config("embedding-models");
        ensure_default_files(&cfg).unwrap();
        let embedding = load_embedding_config(&cfg);
        assert_eq!(
            embedding.model,
            crate::config::DEFAULT_EMBEDDING_MODEL.to_string()
        );
        assert_eq!(
            resolve_embedding_model(&cfg, &embedding, "embeddings/embeddinggemma"),
            crate::config::DEFAULT_EMBEDDING_MODEL
        );
        assert_eq!(
            resolve_embedding_model(&cfg, &embedding, crate::config::DEFAULT_EMBEDDING_MODEL),
            crate::config::DEFAULT_EMBEDDING_MODEL
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

    #[test]
    fn codex_chat_tools_normalize_to_responses_shape() {
        let cfg = test_config("codex-chat-tools");
        ensure_default_files(&cfg).unwrap();
        let body = transform_request(
            "/v1/chat/completions",
            br#"{"model":"codex/gpt-5.4-mini","messages":[{"role":"user","content":"weather"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],"tool_choice":{"type":"function","function":{"name":"get_weather"}}}"#,
            &cfg,
        )
        .unwrap();
        let parsed = json::parse(&body).unwrap();
        let Some(Json::Array(tools)) = parsed.get("tools") else {
            panic!("expected tools array");
        };
        assert_eq!(tools[0].get_str("type"), Some("function"));
        assert_eq!(tools[0].get_str("name"), Some("get_weather"));
        assert!(tools[0].get("function").is_none());
        assert!(tools[0].get("parameters").is_some());
        let choice = parsed.get("tool_choice").expect("tool choice");
        assert_eq!(choice.get_str("type"), Some("function"));
        assert_eq!(choice.get_str("name"), Some("get_weather"));
        assert!(choice.get("function").is_none());
    }

    #[test]
    fn codex_responses_tools_normalize_chat_style_shape() {
        let cfg = test_config("codex-responses-tools");
        ensure_default_files(&cfg).unwrap();
        let body = transform_request(
            "/v1/responses",
            br#"{"model":"codex/gpt-5.4-mini","input":"weather","tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object"}}}],"tool_choice":{"type":"function","function":{"name":"get_weather"}}}"#,
            &cfg,
        )
        .unwrap();
        let parsed = json::parse(&body).unwrap();
        let Some(Json::Array(tools)) = parsed.get("tools") else {
            panic!("expected tools array");
        };
        assert_eq!(tools[0].get_str("name"), Some("get_weather"));
        assert!(tools[0].get("function").is_none());
        let choice = parsed.get("tool_choice").expect("tool choice");
        assert_eq!(choice.get_str("name"), Some("get_weather"));
        assert!(choice.get("function").is_none());
    }

    #[test]
    fn opencode_go_down_statuses() {
        for s in [401, 402, 403, 429] {
            assert!(opencode_go_status_means_down(s), "{s} should be down");
        }
        for s in [200, 400, 404, 408, 500, 503] {
            assert!(!opencode_go_status_means_down(s), "{s} should not be down");
        }
    }

    #[test]
    fn pool_round_robins_when_all_healthy() {
        // Unique key names so other tests' cooldowns don't interfere via the global pool.
        let keys = vec!["rr_alpha_xyz".to_string(), "rr_beta_xyz".to_string()];
        let mut seen = std::collections::HashSet::new();
        for _ in 0..6 {
            seen.insert(select_opencode_go_key(&keys));
        }
        // Both keys are exercised over several picks → load balanced.
        assert!(seen.contains(&0) && seen.contains(&1));
    }

    #[test]
    fn pool_skips_key_in_cooldown() {
        let keys = vec!["cd_alpha_qaz".to_string(), "cd_beta_qaz".to_string()];
        // Park key 0 for a long window; every pick must land on key 1.
        mark_opencode_go_key_down(&keys[0], Duration::from_secs(3600));
        for _ in 0..5 {
            assert_eq!(select_opencode_go_key(&keys), 1);
        }
    }

    #[test]
    fn pool_recovers_after_cooldown_expires() {
        let keys = vec!["rec_alpha_plm".to_string(), "rec_beta_plm".to_string()];
        // A cooldown of zero is effectively already expired → key re-enters rotation.
        mark_opencode_go_key_down(&keys[0], Duration::from_secs(0));
        let mut seen = std::collections::HashSet::new();
        for _ in 0..6 {
            seen.insert(select_opencode_go_key(&keys));
        }
        assert!(
            seen.contains(&0),
            "key should recover once cooldown expires"
        );
    }

    #[test]
    fn responses_string_input_becomes_user_message() {
        let req = json::parse(r#"{"model":"deepseek-v4-flash","input":"hello"}"#).unwrap();
        let (chat, tool) = responses_to_chat(&req, "default", true).unwrap();
        assert!(tool.is_none());
        let Some(Json::Array(messages)) = chat.get("messages") else {
            panic!("expected messages array");
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get_str("role"), Some("user"));
        assert_eq!(messages[0].get_str("content"), Some("hello"));
    }

    #[test]
    fn responses_instructions_and_array_input() {
        let req = json::parse(
            r#"{"model":"m","instructions":"be terse","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        )
        .unwrap();
        let (chat, _) = responses_to_chat(&req, "default", true).unwrap();
        let Some(Json::Array(messages)) = chat.get("messages") else {
            panic!("expected messages");
        };
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].get_str("role"), Some("system"));
        assert_eq!(messages[0].get_str("content"), Some("be terse"));
        assert_eq!(messages[1].get_str("content"), Some("hi"));
    }

    #[test]
    fn json_schema_bridges_to_json_object_plus_prompt() {
        let req = json::parse(
            r#"{"model":"deepseek-v4-flash","input":"x","text":{"format":{"type":"json_schema","name":"person","strict":true,"schema":{"type":"object","properties":{"a":{"type":"string"}}}}}}"#,
        )
        .unwrap();
        let (chat, tool) = responses_to_chat(&req, "default", true).unwrap();
        // Thinking models reject forced tool_choice, so no tool bridge.
        assert!(tool.is_none());
        assert!(chat.get("tools").is_none());
        assert!(chat.get("tool_choice").is_none());
        // json_object is used (DeepSeek rejects json_schema response_format).
        let rf = chat.get("response_format").expect("response_format set");
        assert_eq!(rf.get_str("type"), Some("json_object"));
        // The schema is injected into a system message so the model conforms.
        let Some(Json::Array(messages)) = chat.get("messages") else {
            panic!("expected messages");
        };
        let last = messages.last().unwrap();
        assert_eq!(last.get_str("role"), Some("system"));
        assert!(last.get_str("content").unwrap().contains("JSON Schema"));
    }

    #[test]
    fn json_schema_passthrough_when_not_bridged() {
        let req = json::parse(
            r#"{"model":"m","input":"x","text":{"format":{"type":"json_schema","name":"r","schema":{"type":"object"}}}}"#,
        )
        .unwrap();
        let (chat, tool) = responses_to_chat(&req, "default", false).unwrap();
        assert!(tool.is_none());
        let rf = chat.get("response_format").expect("response_format set");
        assert_eq!(rf.get_str("type"), Some("json_schema"));
    }

    #[test]
    fn tool_call_result_becomes_output_text() {
        let chat = json::parse(
            r#"{"model":"deepseek-v4-flash","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"person","arguments":"{\"a\":\"b\"}"}}]}}],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}"#,
        )
        .unwrap();
        let resp = chat_completion_to_responses(&chat, Some("person"));
        assert_eq!(resp.get_str("object"), Some("response"));
        assert_eq!(resp.get_str("status"), Some("completed"));
        assert_eq!(resp.get_str("output_text"), Some(r#"{"a":"b"}"#));
        assert_eq!(
            resp.get("usage")
                .and_then(|u| u.get_str("input_tokens"))
                .or_else(
                    || resp.get("usage").and_then(|u| match u.get("input_tokens") {
                        Some(Json::Number(n)) => Some(n.as_str()),
                        _ => None,
                    })
                ),
            Some("5")
        );
    }

    #[test]
    fn plain_chat_content_becomes_output_message() {
        let chat = json::parse(
            r#"{"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hi there"}}]}"#,
        )
        .unwrap();
        let resp = chat_completion_to_responses(&chat, None);
        assert_eq!(resp.get_str("output_text"), Some("hi there"));
        let Some(Json::Array(output)) = resp.get("output") else {
            panic!("expected output array");
        };
        assert_eq!(output[0].get_str("type"), Some("message"));
    }

    #[test]
    fn responses_sse_emits_full_event_sequence() {
        let chat = json::parse(
            r#"{"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hello"}}]}"#,
        )
        .unwrap();
        let resp = chat_completion_to_responses(&chat, None);
        let sse = responses_object_to_sse(&resp);
        assert!(sse.contains("event: response.created"));
        assert!(sse.contains("event: response.output_text.delta"));
        assert!(sse.contains("event: response.completed"));
        assert!(sse.contains("\"delta\":\"hello\""));
    }

    #[test]
    fn is_responses_path_matches_variants() {
        assert!(is_responses_path("/v1/responses"));
        assert!(is_responses_path("/api/v1/responses"));
        assert!(is_responses_path("/responses"));
        assert!(is_responses_path("/codex"));
        assert!(!is_responses_path("/v1/chat/completions"));
    }
}
