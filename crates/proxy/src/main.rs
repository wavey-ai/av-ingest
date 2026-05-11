use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{Method, Request, Response, StatusCode};
use reqwest::redirect::Policy;
use reqwest::Url;
use std::env;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use tokio::net::lookup_host;
use tracing::{info, warn};
use web_service::{
    load_default_tls_base64, load_tls_base64_from_paths, BodyStream, H2H3Server, HandlerResponse,
    HandlerResult, Router, Server, ServerBuilder, ServerError, StreamWriter, WebSocketHandler,
    WebTransportHandler,
};

const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";

#[derive(Clone)]
struct AppConfig {
    port: u16,
    enable_h3: bool,
    cert_path: Option<String>,
    key_path: Option<String>,
    user_agent: String,
}

impl AppConfig {
    fn from_env() -> Result<Self> {
        Ok(Self {
            port: env_u16("AV_INGEST_PROXY_PORT", 8444)?,
            enable_h3: env_bool("AV_INGEST_PROXY_ENABLE_H3", false),
            cert_path: env::var("AV_INGEST_PROXY_TLS_CERT_PATH").ok(),
            key_path: env::var("AV_INGEST_PROXY_TLS_KEY_PATH").ok(),
            user_agent: env::var("AV_INGEST_PROXY_USER_AGENT")
                .unwrap_or_else(|_| DEFAULT_USER_AGENT.to_string()),
        })
    }

    fn tls_base64(&self) -> Result<(String, String)> {
        match (&self.cert_path, &self.key_path) {
            (Some(cert_path), Some(key_path)) => load_tls_base64_from_paths(cert_path, key_path)
                .with_context(|| {
                    format!(
                        "failed to load TLS PEMs from {} and {}",
                        cert_path, key_path
                    )
                }),
            (None, None) => load_default_tls_base64()
                .context("failed to load default local Wavey TLS certificate"),
            _ => anyhow::bail!(
                "set both AV_INGEST_PROXY_TLS_CERT_PATH and AV_INGEST_PROXY_TLS_KEY_PATH, or neither"
            ),
        }
    }
}

struct MediaProxy {
    client: reqwest::Client,
    user_agent: String,
}

impl MediaProxy {
    fn new(user_agent: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(120))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_while_idle(true)
            .redirect(Policy::none())
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .context("failed to build upstream HTTP client")?;
        Ok(Self { client, user_agent })
    }

    fn healthz(&self) -> HandlerResponse {
        HandlerResponse {
            status: StatusCode::OK,
            body: Some(Bytes::from_static(
                br#"{"status":"ok","service":"av-ingest-proxy"}"#,
            )),
            content_type: Some("application/json".to_string()),
            headers: base_response_headers(),
            etag: None,
        }
    }

    fn options(&self) -> HandlerResponse {
        HandlerResponse {
            status: StatusCode::NO_CONTENT,
            body: None,
            content_type: None,
            headers: base_response_headers(),
            etag: None,
        }
    }

    async fn proxy_media(
        &self,
        req: Request<()>,
        mut writer: Box<dyn StreamWriter>,
    ) -> HandlerResult<()> {
        if req.method() == Method::OPTIONS {
            return self
                .write_empty_stream(writer, StatusCode::NO_CONTENT)
                .await;
        }
        if req.method() != Method::GET && req.method() != Method::HEAD {
            return self
                .write_text_stream(writer, StatusCode::METHOD_NOT_ALLOWED, "Method not allowed")
                .await;
        }

        let upstream_url = match target_url(&req) {
            Ok(url) => url,
            Err(error) => {
                return self
                    .write_text_stream(writer, StatusCode::BAD_REQUEST, &error)
                    .await
            }
        };

        let response = match self
            .fetch_upstream(req.method(), req.headers(), upstream_url)
            .await
        {
            Ok(response) => response,
            Err(ProxyFetchError::Forbidden(error)) => {
                return self
                    .write_text_stream(writer, StatusCode::FORBIDDEN, &error)
                    .await
            }
            Err(ProxyFetchError::BadGateway(error)) => {
                return self
                    .write_text_stream(
                        writer,
                        StatusCode::BAD_GATEWAY,
                        &format!("Upstream fetch failed: {error}"),
                    )
                    .await;
            }
        };

        if req.method() == Method::GET && is_hls_response(&response) {
            return self.write_hls_playlist(writer, response).await;
        }

        let head = streaming_head(&response)?;
        writer.send_response(head).await?;

        if req.method() == Method::HEAD {
            return writer.finish().await;
        }

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(reqwest_error)?;
            if !chunk.is_empty() {
                writer.send_data(chunk).await?;
            }
        }
        writer.finish().await
    }

    async fn write_hls_playlist(
        &self,
        mut writer: Box<dyn StreamWriter>,
        response: reqwest::Response,
    ) -> HandlerResult<()> {
        let status = status_from_reqwest(response.status())?;
        let base_url = response.url().clone();
        let body = response.bytes().await.map_err(reqwest_error)?;
        let text = String::from_utf8_lossy(&body);
        let body = if text.trim_start().starts_with("#EXTM3U") {
            Bytes::from(rewrite_hls_playlist(&text, &base_url))
        } else {
            body
        };

        let response = Response::builder()
            .status(status)
            .header("content-type", "application/vnd.apple.mpegurl")
            .header("content-length", body.len().to_string())
            .header("cache-control", "no-store")
            .header("x-handled-by", "av-ingest-proxy")
            .header("access-control-allow-origin", "*")
            .header("access-control-allow-methods", "GET, HEAD, OPTIONS")
            .header("access-control-allow-headers", "Range, Content-Type, If-Range")
            .header("access-control-allow-private-network", "true")
            .header(
                "access-control-expose-headers",
                "accept-ranges, content-length, content-range, content-type, etag, last-modified, x-handled-by",
            )
            .body(())?;
        writer.send_response(response).await?;
        writer.send_data(body).await?;
        writer.finish().await
    }

    async fn fetch_upstream(
        &self,
        method: &Method,
        headers: &http::HeaderMap,
        initial_url: Url,
    ) -> Result<reqwest::Response, ProxyFetchError> {
        let mut upstream_url = initial_url;
        for _ in 0..=5 {
            validate_upstream_url(&upstream_url).map_err(ProxyFetchError::Forbidden)?;
            validate_resolved_upstream_host(&upstream_url)
                .await
                .map_err(ProxyFetchError::Forbidden)?;

            let mut upstream = self
                .client
                .request(method.clone(), upstream_url.clone())
                .header(reqwest::header::USER_AGENT, &self.user_agent);

            for name in [
                "accept",
                "if-range",
                "range",
                "referer",
                "sec-fetch-dest",
                "sec-fetch-mode",
                "sec-fetch-site",
            ] {
                if let Some(value) = headers.get(name) {
                    upstream = upstream.header(name, value);
                }
            }

            let response = upstream.send().await.map_err(|error| {
                warn!(%error, target = %upstream_url, "upstream fetch failed");
                ProxyFetchError::BadGateway(error.to_string())
            })?;

            if response.status().is_redirection() {
                let Some(location) = response.headers().get("location") else {
                    return Ok(response);
                };
                let location = location.to_str().map_err(|error| {
                    ProxyFetchError::BadGateway(format!("invalid redirect location: {error}"))
                })?;
                upstream_url = upstream_url.join(location).map_err(|error| {
                    ProxyFetchError::BadGateway(format!("invalid redirect URL: {error}"))
                })?;
                continue;
            }

            return Ok(response);
        }
        Err(ProxyFetchError::BadGateway(
            "too many upstream redirects".to_string(),
        ))
    }

    async fn write_empty_stream(
        &self,
        mut writer: Box<dyn StreamWriter>,
        status: StatusCode,
    ) -> HandlerResult<()> {
        let response = Response::builder()
            .status(status)
            .header("cache-control", "no-store")
            .header("x-handled-by", "av-ingest-proxy")
            .header("access-control-allow-origin", "*")
            .header("access-control-allow-methods", "GET, HEAD, OPTIONS")
            .header(
                "access-control-allow-headers",
                "Range, Content-Type, If-Range",
            )
            .body(())?;
        writer.send_response(response).await?;
        writer.finish().await
    }

    async fn write_text_stream(
        &self,
        mut writer: Box<dyn StreamWriter>,
        status: StatusCode,
        message: &str,
    ) -> HandlerResult<()> {
        let response = Response::builder()
            .status(status)
            .header("content-type", "text/plain; charset=utf-8")
            .header("cache-control", "no-store")
            .header("x-handled-by", "av-ingest-proxy")
            .header("access-control-allow-origin", "*")
            .header("access-control-allow-methods", "GET, HEAD, OPTIONS")
            .header(
                "access-control-allow-headers",
                "Range, Content-Type, If-Range",
            )
            .body(())?;
        writer.send_response(response).await?;
        writer
            .send_data(Bytes::from(format!("{}\n", message.trim_end())))
            .await?;
        writer.finish().await
    }
}

enum ProxyFetchError {
    Forbidden(String),
    BadGateway(String),
}

#[async_trait]
impl Router for MediaProxy {
    async fn route(&self, req: Request<()>) -> HandlerResult<HandlerResponse> {
        match req.uri().path() {
            "/healthz" => Ok(self.healthz()),
            "/proxy" if req.method() == Method::OPTIONS => Ok(self.options()),
            "/proxy" => Ok(HandlerResponse {
                status: StatusCode::BAD_REQUEST,
                body: Some(Bytes::from_static(
                    b"/proxy is a streaming endpoint; use GET or HEAD\n",
                )),
                content_type: Some("text/plain; charset=utf-8".to_string()),
                headers: base_response_headers(),
                etag: None,
            }),
            _ => Ok(HandlerResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(Bytes::from_static(b"Not found\n")),
                content_type: Some("text/plain; charset=utf-8".to_string()),
                headers: base_response_headers(),
                etag: None,
            }),
        }
    }

    fn is_streaming(&self, path: &str) -> bool {
        path == "/proxy"
    }

    async fn route_stream(
        &self,
        req: Request<()>,
        writer: Box<dyn StreamWriter>,
    ) -> HandlerResult<()> {
        self.proxy_media(req, writer).await
    }

    async fn route_body(
        &self,
        req: Request<()>,
        body: BodyStream,
    ) -> HandlerResult<HandlerResponse> {
        drop(body);
        self.route(req).await
    }

    fn webtransport_handler(&self) -> Option<&dyn WebTransportHandler> {
        None
    }

    fn websocket_handler(&self, _path: &str) -> Option<&dyn WebSocketHandler> {
        None
    }
}

fn streaming_head(response: &reqwest::Response) -> HandlerResult<Response<()>> {
    let mut builder = Response::builder().status(status_from_reqwest(response.status())?);
    for (name, value) in response.headers() {
        if should_forward_response_header(name.as_str()) {
            builder = builder.header(name.as_str(), value);
        }
    }
    builder = builder
        .header("cache-control", "no-store")
        .header("x-handled-by", "av-ingest-proxy")
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "GET, HEAD, OPTIONS")
        .header("access-control-allow-headers", "Range, Content-Type, If-Range")
        .header("access-control-allow-private-network", "true")
        .header(
            "access-control-expose-headers",
            "accept-ranges, content-length, content-range, content-type, etag, last-modified, x-handled-by",
        );
    builder.body(()).map_err(ServerError::Http)
}

fn is_hls_response(response: &reqwest::Response) -> bool {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    content_type.contains("mpegurl")
        || content_type.contains("vnd.apple.mpegurl")
        || response
            .url()
            .path()
            .to_ascii_lowercase()
            .ends_with(".m3u8")
}

fn rewrite_hls_playlist(input: &str, base_url: &Url) -> String {
    let mut output = String::with_capacity(input.len() + 1024);
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            output.push('\n');
        } else if trimmed.starts_with('#') {
            output.push_str(&rewrite_hls_uri_attributes(line, base_url));
            output.push('\n');
        } else {
            output.push_str(&proxied_hls_uri(trimmed, base_url));
            output.push('\n');
        }
    }
    output
}

fn rewrite_hls_uri_attributes(line: &str, base_url: &Url) -> String {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(index) = rest.find("URI=\"") {
        let (before, _) = rest.split_at(index + 5);
        output.push_str(before);
        let value_start = index + 5;
        let value_and_rest = &rest[value_start..];
        let Some(end) = value_and_rest.find('"') else {
            output.push_str(value_and_rest);
            return output;
        };
        let value = &value_and_rest[..end];
        output.push_str(&proxied_hls_uri(value, base_url));
        output.push('"');
        rest = &value_and_rest[end + 1..];
    }
    output.push_str(rest);
    output
}

fn proxied_hls_uri(value: &str, base_url: &Url) -> String {
    if value.starts_with("data:") || value.starts_with("blob:") || value.starts_with('#') {
        return value.to_string();
    }
    match base_url.join(value) {
        Ok(url) if url.scheme() == "https" || url.scheme() == "http" => {
            format!(
                "/proxy?url={}",
                percent_encode_query_component(url.as_str())
            )
        }
        _ => value.to_string(),
    }
}

fn percent_encode_query_component(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char)
            }
            _ => {
                output.push('%');
                output.push(
                    char::from_digit((byte >> 4) as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
                output.push(
                    char::from_digit((byte & 0x0f) as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
            }
        }
    }
    output
}

fn target_url(req: &Request<()>) -> Result<Url, String> {
    let query = req.uri().query().unwrap_or_default();
    let value = query
        .split('&')
        .find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == "url").then_some(value)
        })
        .ok_or_else(|| "Missing url query parameter".to_string())?;
    let decoded = percent_decode(value)?;
    Url::parse(&decoded).map_err(|error| format!("Invalid url query parameter: {error}"))
}

fn percent_decode(value: &str) -> Result<String, String> {
    let mut out = Vec::with_capacity(value.len());
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        match byte {
            b'+' => out.push(b' '),
            b'%' => {
                let hi = bytes
                    .next()
                    .ok_or_else(|| "Invalid percent escape".to_string())?;
                let lo = bytes
                    .next()
                    .ok_or_else(|| "Invalid percent escape".to_string())?;
                let hex = [hi, lo];
                let hex =
                    std::str::from_utf8(&hex).map_err(|_| "Invalid percent escape".to_string())?;
                let decoded = u8::from_str_radix(hex, 16)
                    .map_err(|_| "Invalid percent escape".to_string())?;
                out.push(decoded);
            }
            _ => out.push(byte),
        }
    }
    String::from_utf8(out).map_err(|_| "URL parameter was not UTF-8".to_string())
}

fn validate_upstream_url(url: &Url) -> Result<(), String> {
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err("Unsupported URL protocol".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL must include a host".to_string())?;
    if is_blocked_host(host) {
        return Err(format!("Host not allowed: {host}"));
    }
    if !is_allowed_host(host) && !is_direct_media_path(url.path()) {
        return Err(format!("Host not allowed: {host}"));
    }
    Ok(())
}

async fn validate_resolved_upstream_host(url: &Url) -> Result<(), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL must include a host".to_string())?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }

    let port = url.port_or_known_default().unwrap_or(443);
    let resolved = lookup_host((host, port))
        .await
        .map_err(|error| format!("Could not resolve upstream host: {error}"))?;
    let addrs: Vec<_> = resolved.collect();
    if addrs.is_empty() {
        return Err("Upstream host did not resolve".to_string());
    }
    for addr in addrs {
        match addr.ip() {
            IpAddr::V4(ip) if is_blocked_ipv4(ip) => {
                return Err(format!("Resolved host address not allowed: {ip}"));
            }
            IpAddr::V6(ip) if is_blocked_ipv6(ip) => {
                return Err(format!("Resolved host address not allowed: {ip}"));
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_allowed_host(hostname: &str) -> bool {
    let host = hostname.to_ascii_lowercase();
    [
        "youtube.com",
        "www.youtube.com",
        "m.youtube.com",
        "music.youtube.com",
        "youtube-nocookie.com",
        "www.youtube-nocookie.com",
        "youtu.be",
        "googlevideo.com",
        "soundcloud.com",
        "api-v2.soundcloud.com",
        "sndcdn.com",
    ]
    .iter()
    .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

fn is_direct_media_path(path: &str) -> bool {
    [".mp4", ".m4v", ".mov", ".webm", ".m3u8", ".mpd"]
        .iter()
        .any(|suffix| path.to_ascii_lowercase().ends_with(suffix))
}

fn is_blocked_host(hostname: &str) -> bool {
    let host = hostname
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "0.0.0.0") {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(ip) => is_blocked_ipv4(ip),
            IpAddr::V6(ip) => is_blocked_ipv6(ip),
        };
    }
    false
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || ip.is_unspecified()
        || is_shared_carrier_ipv4(ip)
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast()
        || is_documentation_ipv6(ip)
}

fn is_shared_carrier_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn is_documentation_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] == 0x2001 && segments[1] == 0x0db8
}

fn should_forward_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept-ranges"
            | "content-disposition"
            | "content-length"
            | "content-range"
            | "content-type"
            | "etag"
            | "expires"
            | "last-modified"
    )
}

fn base_response_headers() -> Vec<(String, String)> {
    vec![
        ("cache-control".to_string(), "no-store".to_string()),
        ("x-handled-by".to_string(), "av-ingest-proxy".to_string()),
        (
            "access-control-allow-private-network".to_string(),
            "true".to_string(),
        ),
        (
            "access-control-expose-headers".to_string(),
            "accept-ranges, content-length, content-range, content-type, etag, last-modified, x-handled-by"
                .to_string(),
        ),
    ]
}

fn status_from_reqwest(status: reqwest::StatusCode) -> Result<StatusCode, ServerError> {
    StatusCode::from_u16(status.as_u16()).map_err(|error| {
        ServerError::Config(format!("failed to map upstream status {}: {error}", status))
    })
}

fn reqwest_error(error: reqwest::Error) -> ServerError {
    ServerError::Handler(Box::new(error))
}

fn env_bool(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(value) => matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"),
        Err(_) => default,
    }
}

fn env_u16(name: &str, default: u16) -> Result<u16> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u16>()
            .with_context(|| format!("failed to parse {name}={value} as u16")),
        Err(_) => Ok(default),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "av_ingest_proxy=info,web_service=info".into()),
        )
        .init();

    let config = AppConfig::from_env()?;
    let (cert, key) = config.tls_base64()?;
    let router = Box::new(MediaProxy::new(config.user_agent.clone())?);
    let server = H2H3Server::builder()
        .with_tls(cert, key)
        .with_port(config.port)
        .enable_h2(true)
        .enable_h3(config.enable_h3)
        .enable_websocket(false)
        .with_router(router)
        .build()?;

    let handle = server.start().await?;
    let _ = handle.ready_rx.await;
    info!(
        port = config.port,
        h3 = config.enable_h3,
        "av ingest proxy ready"
    );

    tokio::signal::ctrl_c().await?;
    info!("shutting down av ingest proxy");
    let _ = handle.shutdown_tx.send(());
    let _ = handle.finished_rx.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_googlevideo_hosts() {
        let url = Url::parse("https://rr1---sn-aigzrnss.googlevideo.com/videoplayback").unwrap();
        assert!(validate_upstream_url(&url).is_ok());
    }

    #[test]
    fn blocks_private_hosts() {
        let url = Url::parse("http://127.0.0.1/video.mp4").unwrap();
        assert!(validate_upstream_url(&url).is_err());
    }

    #[test]
    fn blocks_private_ipv6_hosts() {
        let url = Url::parse("http://[fd00::1]/video.mp4").unwrap();
        assert!(validate_upstream_url(&url).is_err());
    }

    #[test]
    fn blocks_carrier_grade_nat_hosts() {
        let url = Url::parse("http://100.64.1.1/video.mp4").unwrap();
        assert!(validate_upstream_url(&url).is_err());
    }

    #[test]
    fn allows_direct_media_paths() {
        let url = Url::parse("https://cdn.example.com/path/video.mp4").unwrap();
        assert!(validate_upstream_url(&url).is_ok());
    }

    #[test]
    fn decodes_query_url() {
        let req = Request::builder()
            .uri("/proxy?url=https%3A%2F%2Fexample.com%2Fvideo.mp4%3Fa%3D1")
            .body(())
            .unwrap();
        assert_eq!(
            target_url(&req).unwrap().as_str(),
            "https://example.com/video.mp4?a=1"
        );
    }

    #[test]
    fn rewrites_hls_playlist_urls_through_proxy() {
        let base =
            Url::parse("https://manifest.googlevideo.com/api/manifest/hls_playlist/abc").unwrap();
        let rewritten = rewrite_hls_playlist(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\nseg-1.m4s\nhttps://rr1---sn.googlevideo.com/videoplayback?id=1&itag=137\n",
            &base,
        );
        assert!(rewritten.contains("#EXT-X-MAP:URI=\"/proxy?url=https%3A%2F%2Fmanifest.googlevideo.com%2Fapi%2Fmanifest%2Fhls_playlist%2Finit.mp4\""));
        assert!(rewritten.contains("/proxy?url=https%3A%2F%2Fmanifest.googlevideo.com%2Fapi%2Fmanifest%2Fhls_playlist%2Fseg-1.m4s"));
        assert!(rewritten.contains("/proxy?url=https%3A%2F%2Frr1---sn.googlevideo.com%2Fvideoplayback%3Fid%3D1%26itag%3D137"));
    }
}
