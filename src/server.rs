//! A tiny read-only HTTP/1.1 server on 127.0.0.1 that serves:
//!   GET /kimo.svg       -> the input SVG (Content-Type: image/svg+xml)
//!   GET /<anything>     -> sibling files of the SVG (guarded against traversal)
//!
//! Chrome loads the SVG as a proper `image/svg+xml` document from an http
//! origin, so relative hrefs, external fonts, CSS and scripts all resolve
//! exactly like they would on a real site (file:// would break several of
//! those). The SVG itself is served byte-for-byte — no rewriting ever.

use crate::error::{KimoError, Result};
use percent_encoding::percent_decode_str;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
pub struct ServerStats {
    pub requests: AtomicU64,
    pub misses: AtomicU64,
    pub missed_paths: std::sync::Mutex<Vec<String>>,
}

pub struct AssetServer {
    pub port: u16,
    pub base_url: String,
    stats: Arc<ServerStats>,
    handle: tokio::task::JoinHandle<()>,
}

impl AssetServer {
    pub async fn start(svg_path: &Path, svg_bytes: Vec<u8>) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| KimoError::Io(e))?;
        let port = listener
            .local_addr()
            .map_err(|e| KimoError::Io(e))?
            .port();

        let root = svg_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let svg_name = svg_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "kimo.svg".into());

        let stats = Arc::new(ServerStats::default());
        let stats2 = stats.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let svg_bytes = svg_bytes.clone();
                let root = root.clone();
                let svg_name = svg_name.clone();
                let stats = stats2.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(stream, svg_bytes, root, svg_name, stats).await;
                });
            }
        });

        Ok(AssetServer {
            port,
            base_url: format!("http://127.0.0.1:{port}/kimo.svg"),
            stats,
            handle,
        })
    }

    pub fn stats(&self) -> (u64, Vec<String>) {
        let misses = self.stats.misses.load(Ordering::Relaxed);
        let paths = self.stats.missed_paths.lock().unwrap().clone();
        (self.stats.requests.load(Ordering::Relaxed), {
            let _ = misses;
            paths
        })
    }

    pub fn shutdown(self) {
        self.handle.abort();
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    svg_bytes: Vec<u8>,
    root: PathBuf,
    svg_name: String,
    stats: Arc<ServerStats>,
) -> std::io::Result<()> {
    // Serve a few sequential requests per connection; close on error.
    let _ = stream.set_nodelay(true);
    loop {
        // Read request head until CRLFCRLF.
        let mut buf: Vec<u8> = Vec::with_capacity(1024);
        let mut chunk = [0u8; 2048];
        'head: loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
            if find_subslice(&buf, b"\r\n\r\n").is_some() || buf.len() > 64 * 1024 {
                break 'head;
            }
        }
        let head = String::from_utf8_lossy(&buf);
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let raw_target = parts.next().unwrap_or("/");
        let path = raw_target.split(['?', '#']).next().unwrap_or("/");

        if method != "GET" && method != "HEAD" {
            respond(&mut stream, 405, "text/plain", b"method not allowed", method == "HEAD").await?;
            return Ok(());
        }

        stats.requests.fetch_add(1, Ordering::Relaxed);

        if path == "/kimo.svg" {
            respond(&mut stream, 200, "image/svg+xml; charset=utf-8", &svg_bytes, method == "HEAD")
                .await?;
            continue;
        }

        // Everything else: a sibling asset of the SVG, resolved safely.
        let decoded = percent_decode_str(path).decode_utf8_lossy().to_string();
        let decoded = decoded.trim_start_matches('/');
        if decoded.is_empty() || decoded.contains('\0') {
            respond(&mut stream, 404, "text/plain", b"not found", method == "HEAD").await?;
            return Ok(());
        }
        // Block traversal before touching the filesystem.
        if decoded.split('/').any(|seg| seg == "..") {
            respond(&mut stream, 403, "text/plain", b"forbidden", method == "HEAD").await?;
            return Ok(());
        }
        let candidate = root.join(decoded);
        let canonical = std::fs::canonicalize(&candidate).ok();
        let inside = canonical
            .as_ref()
            .map(|c| c.starts_with(&root))
            .unwrap_or(false);
        // The SVG itself may also be referenced relatively by its own name.
        let is_svg_itself = decoded == svg_name;
        if !inside && !is_svg_itself {
            record_miss(&stats, decoded);
            respond(&mut stream, 404, "text/plain", b"not found", method == "HEAD").await?;
            continue;
        }
        let body = if is_svg_itself {
            svg_bytes.clone()
        } else {
            match tokio::fs::read(&candidate).await {
                Ok(b) => b,
                Err(_) => {
                    record_miss(&stats, decoded);
                    respond(&mut stream, 404, "text/plain", b"not found", method == "HEAD").await?;
                    continue;
                }
            }
        };
        let mime = mime_for(decoded);
        respond(&mut stream, 200, mime, &body, method == "HEAD").await?;
    }
}

fn record_miss(stats: &Arc<ServerStats>, path: &str) {
    stats.misses.fetch_add(1, Ordering::Relaxed);
    let mut missed = stats.missed_paths.lock().unwrap();
    if missed.len() < 20 {
        missed.push(path.to_string());
    }
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    if !head_only {
        stream.write_all(body).await?;
    }
    stream.flush().await
}

fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "svg" | "svgz" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogg" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "txt" => "text/plain",
        "xml" => "text/xml",
        _ => "application/octet-stream",
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
