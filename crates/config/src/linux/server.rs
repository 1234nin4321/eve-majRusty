//! A tiny HTTP server on 127.0.0.1 that serves the dialog page and answers its backend calls.
//!
//! Calls are `POST /call/<name>` with a JSON array of arguments and must carry the per-run token in an `X-EveMaj-Token`
//! header. A custom header can't be sent cross-origin without a CORS preflight (which is refused), and the Host and
//! Origin headers are checked too, so other local processes' web pages can't drive the backend.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use eve_maj_core::log::Scope;

const SLOG: Scope = Scope::new("config-server");
const MAX_BODY: usize = 16 * 1024 * 1024;

pub struct Asset {
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

pub trait Handler: Send + 'static {
    /// A static file at `path` ("/" for the page), if there is one.
    fn asset(&self, path: &str) -> Option<Asset>;
    /// The reply to `webui.call(name, ...args)`.
    fn call(&mut self, name: &str, args: Vec<serde_json::Value>) -> String;
    /// The reply to a call that needs no handler state, answered without taking the handler's lock so a slow one
    /// (a network request) doesn't hold up every other call; None for calls `call` handles.
    fn call_without_state(_name: &str) -> Option<String>
    where
        Self: Sized,
    {
        None
    }
}

pub struct Server {
    pub port: u16,
    pub token: String,
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Server {
    pub fn start(handler: impl Handler) -> std::io::Result<Server> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        // Debugging/testing only: a fixed token (and the port on stderr) lets tools call the backend directly.
        let token = match std::env::var("EVE_MAJ_CONFIG_TOKEN").ok().filter(|t| t.len() >= 16) {
            Some(t) => {
                eprintln!("eve-maj-config: backend on 127.0.0.1:{port} with the token from EVE_MAJ_CONFIG_TOKEN");
                t
            }
            None => random_token(),
        };
        let handler = Arc::new(Mutex::new(handler));
        let t = token.clone();
        std::thread::Builder::new().name("config-server".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = handler.clone();
                let token = t.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve(stream, port, &token, &handler) {
                        SLOG.debug(format_args!("Request failed: {e}"));
                    }
                });
            }
        })?;
        Ok(Server { port, token })
    }
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn serve<H: Handler>(mut stream: TcpStream, port: u16, token: &str, handler: &Mutex<H>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());

    // DNS rebinding: only our own host name is served.
    let host_ok = header("host").is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
    if !host_ok {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"forbidden");
    }
    let path = target.split('?').next().unwrap_or("/");

    match method {
        "GET" => {
            let asset = handler.lock().unwrap_or_else(|p| p.into_inner()).asset(path);
            match asset {
                Some(a) => respond(&mut stream, "200 OK", a.content_type, &a.body),
                None => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
            }
        }
        "POST" if path.starts_with("/call/") => {
            let origin_ok = header("origin").is_none_or(|o| o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}"));
            if !origin_ok || header("x-evemaj-token") != Some(token) {
                return respond(&mut stream, "403 Forbidden", "text/plain", b"forbidden");
            }
            let len: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            if len > MAX_BODY {
                return respond(&mut stream, "413 Payload Too Large", "text/plain", b"too large");
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body)?;
            let args: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap_or_default();
            let name = &path["/call/".len()..];
            let reply = match H::call_without_state(name) {
                Some(reply) => reply,
                None => handler.lock().unwrap_or_else(|p| p.into_inner()).call(name, args),
            };
            respond(&mut stream, "200 OK", "text/plain; charset=utf-8", reply.as_bytes())
        }
        _ => respond(&mut stream, "405 Method Not Allowed", "text/plain", b"method not allowed"),
    }
}
