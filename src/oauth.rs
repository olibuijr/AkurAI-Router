use crate::auth;
use crate::config::Config;
use crate::http::{self, Request};
use crate::json;
use crate::upstream;
use crate::util::{percent_encode, query_get, random_hex};

pub fn login(_req: &Request, stream: &mut std::net::TcpStream, cfg: &Config) {
    let state = match auth::create_oauth_state(cfg) {
        Ok(s) => s,
        Err(e) => {
            let _ = http::send_text(stream, 500, "text/plain", &e);
            return;
        }
    };
    let nonce = random_hex(16);
    let location = format!(
        "{}/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}",
        cfg.idp_issuer,
        percent_encode(&cfg.idp_client_id),
        percent_encode(&cfg.callback_url()),
        percent_encode("openid profile email groups"),
        percent_encode(&state),
        percent_encode(&nonce),
    );
    let _ = http::redirect(stream, &location, &[auth::state_cookie(cfg, &state)]);
}

pub fn callback(req: &Request, stream: &mut std::net::TcpStream, cfg: &Config) {
    let query = crate::util::parse_query(&req.query);
    let code = query_get(&query, "code").unwrap_or_default();
    let state = query_get(&query, "state").unwrap_or_default();
    if code.is_empty() || state.is_empty() || !auth::validate_oauth_state(req, cfg, &state) {
        let _ = http::send_text(stream, 400, "text/html", "<h1>Invalid login callback</h1>");
        return;
    }

    let token_body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}",
        percent_encode(&code),
        percent_encode(&cfg.callback_url()),
        percent_encode(&cfg.idp_client_id),
        percent_encode(&cfg.idp_client_secret),
    );
    let token = match upstream::curl_capture(
        "POST",
        &format!("{}/token", cfg.idp_issuer),
        &[
            (
                "Content-Type",
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Accept", "application/json".to_string()),
        ],
        token_body.as_bytes(),
        30,
    ) {
        Ok(resp) if resp.status == 200 => match json::parse(&String::from_utf8_lossy(&resp.body)) {
            Ok(v) => v,
            Err(e) => {
                let _ = http::send_text(
                    stream,
                    502,
                    "text/plain",
                    &format!("invalid IDP token JSON: {e}"),
                );
                return;
            }
        },
        Ok(resp) => {
            let _ = http::send_text(
                stream,
                502,
                "text/plain",
                &format!(
                    "IDP token exchange failed: {}",
                    String::from_utf8_lossy(&resp.body)
                ),
            );
            return;
        }
        Err(e) => {
            let _ = http::send_text(stream, 502, "text/plain", &e);
            return;
        }
    };

    let access = token.get_str("access_token").unwrap_or("");
    if access.is_empty() {
        let _ = http::send_text(stream, 502, "text/plain", "IDP did not return access_token");
        return;
    }

    let userinfo = match upstream::curl_capture(
        "GET",
        &format!("{}/userinfo", cfg.idp_issuer),
        &[
            ("Accept", "application/json".to_string()),
            ("Authorization", format!("Bearer {access}")),
        ],
        b"",
        30,
    ) {
        Ok(resp) if resp.status == 200 => match json::parse(&String::from_utf8_lossy(&resp.body)) {
            Ok(v) => v,
            Err(e) => {
                let _ = http::send_text(
                    stream,
                    502,
                    "text/plain",
                    &format!("invalid userinfo JSON: {e}"),
                );
                return;
            }
        },
        Ok(resp) => {
            let _ = http::send_text(
                stream,
                502,
                "text/plain",
                &format!(
                    "IDP userinfo failed: {}",
                    String::from_utf8_lossy(&resp.body)
                ),
            );
            return;
        }
        Err(e) => {
            let _ = http::send_text(stream, 502, "text/plain", &e);
            return;
        }
    };

    let email = userinfo.get_str("email").unwrap_or("").to_string();
    if !email.eq_ignore_ascii_case(&cfg.admin_allowed_email) {
        let _ = http::send_text(stream, 403, "text/html", "<h1>Access denied</h1>");
        return;
    }
    let session = match auth::create_session(cfg, &email) {
        Ok(s) => s,
        Err(e) => {
            let _ = http::send_text(stream, 500, "text/plain", &e);
            return;
        }
    };
    let _ = http::redirect(
        stream,
        "/admin",
        &[
            auth::session_cookie(cfg, &session),
            auth::clear_state_cookie(cfg),
        ],
    );
}
