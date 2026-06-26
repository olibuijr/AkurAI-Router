use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn cookie(&self, name: &str) -> Option<String> {
        let raw = self.header("cookie")?;
        for part in raw.split(';') {
            let mut pieces = part.trim().splitn(2, '=');
            if pieces.next()? == name {
                return Some(pieces.next().unwrap_or("").to_string());
            }
        }
        None
    }
}

pub fn serve<F>(addr: &str, handler: F) -> Result<(), String>
where
    F: Fn(Request, &mut TcpStream) + Send + Sync + 'static,
{
    let listener = TcpListener::bind(addr).map_err(|e| format!("failed to bind {addr}: {e}"))?;
    eprintln!("akurai-router listening on http://{addr}");
    let handler = Arc::new(handler);
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let handler = Arc::clone(&handler);
                thread::spawn(move || match read_request(&mut stream) {
                    Ok(req) => handler(req, &mut stream),
                    Err(err) => {
                        let _ = send_text(&mut stream, 400, "text/plain", &err);
                    }
                });
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
    Ok(())
}

pub fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before request".to_string());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            header_end = pos;
            break;
        }
        if buf.len() > 1024 * 1024 {
            return Err("request header too large".to_string());
        }
    }

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "missing request line".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let value = v.trim().to_string();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((key, value));
        }
    }
    if content_length > 64 * 1024 * 1024 {
        return Err("request body too large".to_string());
    }

    let body_start = header_end + 4;
    let mut body = if body_start <= buf.len() {
        buf[body_start..].to_vec()
    } else {
        Vec::new()
    };
    while body.len() < content_length {
        let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed during body".to_string());
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

pub fn send_response(
    stream: &mut TcpStream,
    status: u16,
    status_text: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {status_text}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )?;
    for (key, value) in headers {
        write!(stream, "{key}: {value}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn send_text(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    send_response(
        stream,
        status,
        status_text(status),
        &[("Content-Type", content_type.to_string()), cors()],
        body.as_bytes(),
    )
}

pub fn send_json(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    send_text(stream, status, "application/json", body)
}

pub fn redirect(
    stream: &mut TcpStream,
    location: &str,
    extra_headers: &[(&str, String)],
) -> std::io::Result<()> {
    let mut headers = vec![
        ("Location", location.to_string()),
        ("Content-Type", "text/plain".to_string()),
    ];
    headers.extend_from_slice(extra_headers);
    send_response(stream, 302, "Found", &headers, b"Found")
}

pub fn no_content(stream: &mut TcpStream) -> std::io::Result<()> {
    send_response(
        stream,
        204,
        "No Content",
        &[
            cors(),
            (
                "Access-Control-Allow-Methods",
                "GET, POST, OPTIONS".to_string(),
            ),
            ("Access-Control-Allow-Headers", "*".to_string()),
        ],
        b"",
    )
}

pub fn stream_headers(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\nConnection: close\r\n",
        status_text(status)
    )?;
    let mut has_type = false;
    for (key, value) in headers {
        if key.eq_ignore_ascii_case("content-length")
            || key.eq_ignore_ascii_case("transfer-encoding")
            || key.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        if key.eq_ignore_ascii_case("content-type") {
            has_type = true;
        }
        write!(stream, "{key}: {value}\r\n")?;
    }
    if !has_type {
        write!(stream, "Content-Type: text/event-stream\r\n")?;
    }
    write!(stream, "Access-Control-Allow-Origin: *\r\n\r\n")?;
    stream.flush()
}

pub fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "OK",
    }
}

pub fn cors() -> (&'static str, String) {
    ("Access-Control-Allow-Origin", "*".to_string())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}
