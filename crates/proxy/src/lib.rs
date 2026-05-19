mod native_frame;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use reqwest::redirect::Policy;
use reqwest::Url;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::env;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{lookup_host, TcpListener};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use web_service::{
    load_default_tls_base64, load_tls_base64_from_paths, BodyStream, H2H3Server, HandlerResponse,
    HandlerResult, Router, Server, ServerBuilder, ServerError, StreamWriter, WebSocketHandler,
    WebTransportHandler,
};

const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
const YOUTUBE_WEB_API_KEY: &str = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";
const ANDROID_USER_AGENT: &str =
    "com.google.android.youtube/21.03.36(Linux; U; Android 16; en_US; SM-S908E Build/TP1A.220624.014) gzip";
const ANDROID_VR_USER_AGENT: &str =
    "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip";
const IOS_USER_AGENT: &str =
    "com.google.ios.youtube/20.11.6 (iPhone10,4; U; CPU iOS 16_7_7 like Mac OS X)";

#[derive(Clone)]
struct AppConfig {
    port: u16,
    enable_h3: bool,
    local_http: bool,
    cert_path: Option<String>,
    key_path: Option<String>,
    user_agent: String,
    ytdlp: YtDlpConfig,
    resolve_mode: ResolveMode,
}

#[derive(Clone)]
struct YtDlpConfig {
    enabled: bool,
    path: String,
    extractor_args: Option<String>,
    cookies: Option<String>,
    cookies_from_browser: Option<String>,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolveMode {
    Full,
    Transcribe,
}

impl ResolveMode {
    fn from_env() -> Result<Self> {
        Self::parse(
            &env::var("AV_INGEST_PROXY_RESOLVE_MODE").unwrap_or_else(|_| "full".to_string()),
        )
    }

    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "full" => Ok(Self::Full),
            "transcribe" | "audio" | "audio-only" => Ok(Self::Transcribe),
            other => anyhow::bail!(
                "unsupported AV_INGEST_PROXY_RESOLVE_MODE={other}; expected full or transcribe"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Transcribe => "transcribe",
        }
    }

    fn empty_formats_error(self) -> &'static str {
        match self {
            Self::Full => "No browser-playable YouTube video formats found.",
            Self::Transcribe => "No transcribable YouTube audio formats found.",
        }
    }
}

impl AppConfig {
    fn from_env() -> Result<Self> {
        Ok(Self {
            port: env_u16("AV_INGEST_PROXY_PORT", 8444)?,
            enable_h3: env_bool("AV_INGEST_PROXY_ENABLE_H3", false),
            local_http: env_bool("AV_INGEST_PROXY_LOCAL_HTTP", cfg!(debug_assertions)),
            cert_path: env::var("AV_INGEST_PROXY_TLS_CERT_PATH").ok(),
            key_path: env::var("AV_INGEST_PROXY_TLS_KEY_PATH").ok(),
            user_agent: env::var("AV_INGEST_PROXY_USER_AGENT")
                .unwrap_or_else(|_| DEFAULT_USER_AGENT.to_string()),
            ytdlp: YtDlpConfig {
                enabled: env_bool("AV_INGEST_PROXY_YTDLP_ENABLED", true),
                path: env::var("AV_INGEST_PROXY_YTDLP_PATH")
                    .unwrap_or_else(|_| "yt-dlp".to_string()),
                extractor_args: env_nonempty("AV_INGEST_PROXY_YTDLP_EXTRACTOR_ARGS"),
                cookies: env_nonempty("AV_INGEST_PROXY_YTDLP_COOKIES"),
                cookies_from_browser: env_nonempty("AV_INGEST_PROXY_YTDLP_COOKIES_FROM_BROWSER"),
                timeout: Duration::from_secs(env_u64("AV_INGEST_PROXY_YTDLP_TIMEOUT_SECS", 45)?),
            },
            resolve_mode: ResolveMode::from_env()?,
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

#[derive(Clone)]
struct MediaProxy {
    client: reqwest::Client,
    user_agent: String,
    ytdlp: YtDlpConfig,
    resolve_mode: ResolveMode,
}

#[derive(Clone)]
pub struct TranscribeAudioResolver {
    proxy: MediaProxy,
}

pub struct TranscribeAudioStream {
    pub source_url: Url,
    pub duration_seconds: Option<u64>,
    pub resolver: String,
    pub itag: Option<u64>,
    pub mime_type: Option<String>,
    response: reqwest::Response,
}

impl TranscribeAudioResolver {
    pub fn from_env() -> Result<Self> {
        let config = AppConfig::from_env()?;
        Ok(Self {
            proxy: MediaProxy::new(config.user_agent, config.ytdlp, config.resolve_mode)?,
        })
    }

    pub async fn open_youtube_audio(&self, source: &str) -> Result<TranscribeAudioStream> {
        let source_url =
            Url::parse(source).with_context(|| format!("invalid YouTube source URL: {source}"))?;
        self.proxy
            .open_youtube_transcribe_audio(source_url, &Method::GET, &HeaderMap::new())
            .await
    }
}

impl TranscribeAudioStream {
    pub fn response(&self) -> &reqwest::Response {
        &self.response
    }

    pub fn into_response(self) -> reqwest::Response {
        self.response
    }
}

struct ExtractedFrameImage {
    bytes: Vec<u8>,
    content_type: &'static str,
}

#[derive(Clone, Debug)]
struct FrameMediaSource {
    url: Url,
    headers: Vec<(String, String)>,
    resolver: String,
}

#[derive(Clone, Debug)]
struct TranscribeMediaSource {
    url: Url,
    resolver: String,
    itag: Option<u64>,
    mime_type: Option<String>,
}

impl MediaProxy {
    fn new(user_agent: String, ytdlp: YtDlpConfig, resolve_mode: ResolveMode) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .pool_idle_timeout(Duration::from_secs(120))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_while_idle(true)
            .redirect(Policy::none())
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .context("failed to build upstream HTTP client")?;
        Ok(Self {
            client,
            user_agent,
            ytdlp,
            resolve_mode,
        })
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

    async fn fetch_youtube_visitor_data(&self, video_id: &str) -> Result<String, String> {
        let response = self
            .client
            .get(format!("https://www.youtube.com/watch?v={video_id}"))
            .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("watch page HTTP {status}"));
        }
        let html = response.text().await.map_err(|error| error.to_string())?;
        extract_json_string_field(&html, "visitorData")
            .or_else(|| extract_json_string_field(&html, "VISITOR_DATA"))
            .ok_or_else(|| "watch page did not contain visitorData".to_string())
    }

    async fn resolve_source(&self, req: Request<()>) -> HandlerResponse {
        if req.method() == Method::OPTIONS {
            return self.options();
        }
        if req.method() != Method::GET {
            return self.text_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
        }

        let Some(source) = query_param(req.uri().query().unwrap_or_default(), "url") else {
            return self.text_response(StatusCode::BAD_REQUEST, "Missing url query parameter");
        };
        let source = match percent_decode(source) {
            Ok(value) => value,
            Err(error) => return self.text_response(StatusCode::BAD_REQUEST, &error),
        };
        let source_url = match Url::parse(&source) {
            Ok(url) => url,
            Err(error) => {
                return self.text_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid url query parameter: {error}"),
                )
            }
        };
        if !is_youtube_host(source_url.host_str().unwrap_or_default()) {
            return self.json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": "Only YouTube resolve is implemented."}),
            );
        }
        let Some(video_id) = extract_youtube_id(&source_url) else {
            return self.text_response(
                StatusCode::BAD_REQUEST,
                "Could not extract YouTube video id.",
            );
        };

        let resolve_mode =
            query_resolve_mode(req.uri().query().unwrap_or_default()).unwrap_or(self.resolve_mode);

        match self.fetch_best_innertube_player_response(&video_id).await {
            Ok((resolver, mut player_response, attempts)) => {
                if let Some(object) = player_response.as_object_mut() {
                    object.insert(
                        "__avIngestAttempts".to_string(),
                        Value::Array(attempts.clone()),
                    );
                }
                if resolve_mode == ResolveMode::Transcribe {
                    apply_transcribe_resolve_mode(&mut player_response);
                }
                if !player_response_has_formats_for_mode(&player_response, resolve_mode) {
                    return self.json_response(
                        StatusCode::BAD_GATEWAY,
                        json!({
                            "error": resolve_mode.empty_formats_error(),
                            "provider": "youtube",
                            "resolver": resolver,
                            "resolveMode": resolve_mode.as_str(),
                            "watchStatus": Value::Null,
                            "url": source_url.as_str(),
                            "playabilityStatus": player_response.pointer("/playabilityStatus/status").cloned().unwrap_or(Value::Null),
                            "playabilityReason": player_response.pointer("/playabilityStatus/reason").cloned().unwrap_or(Value::Null),
                            "attempts": attempts,
                            "playerResponse": player_response,
                        }),
                    );
                }
                self.json_response(
                    StatusCode::OK,
                    json!({
                        "provider": "youtube",
                        "resolver": resolver,
                        "resolveMode": resolve_mode.as_str(),
                        "watchStatus": Value::Null,
                        "url": source_url.as_str(),
                        "title": player_response.pointer("/videoDetails/title").cloned().unwrap_or(Value::Null),
                        "durationSeconds": player_response
                            .pointer("/videoDetails/lengthSeconds")
                            .and_then(Value::as_str)
                            .and_then(|value| value.parse::<u64>().ok()),
                        "playerChallenge": Value::Null,
                        "playerResponse": player_response,
                    }),
                )
            }
            Err(error) => self.json_response(
                StatusCode::BAD_GATEWAY,
                json!({"error": format!("YouTube resolve failed: {error}")}),
            ),
        }
    }

    fn json_response(&self, status: StatusCode, value: Value) -> HandlerResponse {
        HandlerResponse {
            status,
            body: Some(Bytes::from(format!("{value:#}\n"))),
            content_type: Some("application/json; charset=utf-8".to_string()),
            headers: base_response_headers(),
            etag: None,
        }
    }

    fn text_response(&self, status: StatusCode, message: &str) -> HandlerResponse {
        HandlerResponse {
            status,
            body: Some(Bytes::from(format!("{}\n", message.trim_end()))),
            content_type: Some("text/plain; charset=utf-8".to_string()),
            headers: base_response_headers(),
            etag: None,
        }
    }

    async fn frame_image(&self, req: Request<()>) -> HandlerResponse {
        let started_at = Instant::now();
        if req.method() == Method::OPTIONS {
            return self.options();
        }
        if req.method() != Method::GET {
            return self.text_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
        }

        let query = req.uri().query().unwrap_or_default();
        let Some(source) = query_param(query, "url") else {
            return self.text_response(StatusCode::BAD_REQUEST, "Missing url query parameter");
        };
        let source = match percent_decode(source) {
            Ok(value) => value,
            Err(error) => return self.text_response(StatusCode::BAD_REQUEST, &error),
        };
        let ts_us = match frame_timestamp_us(query) {
            Ok(value) => value,
            Err(error) => return self.text_response(StatusCode::BAD_REQUEST, &error),
        };
        debug!(%source, ts_us, "frame request received");
        let source_url = match Url::parse(&source) {
            Ok(url) => url,
            Err(error) => {
                return self.text_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid url query parameter: {error}"),
                )
            }
        };

        let media_source = if is_youtube_host(source_url.host_str().unwrap_or_default()) {
            let Some(video_id) = extract_youtube_id(&source_url) else {
                return self.text_response(
                    StatusCode::BAD_REQUEST,
                    "Could not extract YouTube video id.",
                );
            };
            debug!(%video_id, ts_us, "resolving youtube source for frame extraction");
            match self.resolve_ytdlp_frame_source(source_url.as_str()).await {
                Ok(source) => {
                    debug!(
                        %video_id,
                        resolver = %source.resolver,
                        host = source.url.host_str().unwrap_or(""),
                        headers = source.headers.len(),
                        "yt-dlp source resolved for frame extraction"
                    );
                    source
                }
                Err(ytdlp_error) => {
                    warn!(
                        %video_id,
                        error = %ytdlp_error,
                        "yt-dlp frame source failed; falling back to InnerTube"
                    );
                    match self.fetch_best_innertube_player_response(&video_id).await {
                        Ok((resolver, player_response, attempts)) => {
                            debug!(
                                %video_id,
                                %resolver,
                                attempts = attempts.len(),
                                "youtube source resolved for frame extraction"
                            );
                            match select_best_innertube_frame_media_source(&player_response, &resolver) {
                                Some(source) => {
                                    debug!(
                                        host = source.url.host_str().unwrap_or(""),
                                        resolver = %source.resolver,
                                        "selected native frame media url"
                                    );
                                    source
                                }
                                None => return self.text_response(
                                    StatusCode::BAD_GATEWAY,
                                    "No direct YouTube video stream was available for frame extraction.",
                                ),
                            }
                        }
                        Err(error) => {
                            warn!(%video_id, %error, "youtube resolve failed for frame extraction");
                            return self.text_response(
                                StatusCode::BAD_GATEWAY,
                                &format!(
                                    "YouTube resolve failed: {error}; yt-dlp failed first: {ytdlp_error}"
                                ),
                            );
                        }
                    }
                }
            }
        } else {
            FrameMediaSource {
                url: source_url,
                headers: Vec::new(),
                resolver: "direct".to_string(),
            }
        };

        if let Err(error) = validate_upstream_url(&media_source.url) {
            return self.text_response(StatusCode::FORBIDDEN, &error);
        }
        if let Err(error) = validate_resolved_upstream_host(&media_source.url).await {
            return self.text_response(StatusCode::FORBIDDEN, &error);
        }

        match self.extract_frame_image(&media_source, ts_us).await {
            Ok(image) => {
                debug!(
                    bytes = image.bytes.len(),
                    content_type = image.content_type,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "frame extraction completed"
                );
                HandlerResponse {
                    status: StatusCode::OK,
                    body: Some(Bytes::from(image.bytes)),
                    content_type: Some(image.content_type.to_string()),
                    headers: base_response_headers(),
                    etag: None,
                }
            }
            Err(error) => {
                warn!(
                    %error,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "frame extraction failed"
                );
                self.text_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Frame extraction failed: {error}"),
                )
            }
        }
    }

    async fn extract_frame_image(
        &self,
        media_source: &FrameMediaSource,
        ts_us: u64,
    ) -> Result<ExtractedFrameImage, String> {
        debug!(
            resolver = %media_source.resolver,
            host = media_source.url.host_str().unwrap_or(""),
            headers = media_source.headers.len(),
            ts_us,
            "starting native frame extraction"
        );
        let bytes = native_frame::extract_vp9_webm_frame_png(
            &self.client,
            &self.user_agent,
            media_source.url.as_str(),
            &media_source.headers,
            ts_us,
        )
        .await?;
        debug!(bytes = bytes.len(), "native frame extraction produced png");
        Ok(ExtractedFrameImage {
            bytes,
            content_type: "image/png",
        })
    }

    async fn open_youtube_transcribe_audio(
        &self,
        source_url: Url,
        method: &Method,
        headers: &HeaderMap,
    ) -> Result<TranscribeAudioStream> {
        anyhow::ensure!(
            is_youtube_host(source_url.host_str().unwrap_or_default()),
            "Only YouTube audio sources are implemented."
        );
        let video_id = extract_youtube_id(&source_url)
            .ok_or_else(|| anyhow::anyhow!("Could not extract YouTube video id."))?;

        let (resolver, mut player_response, _attempts) = self
            .fetch_best_innertube_player_response(&video_id)
            .await
            .map_err(|error| anyhow::anyhow!("YouTube resolve failed: {error}"))?;
        apply_transcribe_resolve_mode(&mut player_response);
        let media_source = select_transcribe_media_source(&player_response, &resolver)
            .ok_or_else(|| anyhow::anyhow!(ResolveMode::Transcribe.empty_formats_error()))?;
        let duration_seconds = player_response
            .pointer("/videoDetails/lengthSeconds")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok());

        debug!(
            source = %source_url,
            upstream = %media_source.url,
            resolver = %media_source.resolver,
            itag = media_source.itag,
            "selected transcribe media stream"
        );

        let response = self
            .fetch_upstream(method, headers, media_source.url)
            .await
            .map_err(|error| match error {
                ProxyFetchError::Forbidden(error) | ProxyFetchError::BadGateway(error) => {
                    anyhow::anyhow!(error)
                }
            })?;

        Ok(TranscribeAudioStream {
            source_url,
            duration_seconds,
            resolver: media_source.resolver,
            itag: media_source.itag,
            mime_type: media_source.mime_type,
            response,
        })
    }

    async fn proxy_media(
        &self,
        req: Request<()>,
        mut writer: Box<dyn StreamWriter>,
    ) -> HandlerResult<()> {
        let origin = request_origin(&req);
        if req.method() == Method::OPTIONS {
            return self
                .write_empty_stream(writer, StatusCode::NO_CONTENT, origin.as_deref())
                .await;
        }
        if req.method() != Method::GET && req.method() != Method::HEAD {
            return self
                .write_text_stream(
                    writer,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "Method not allowed",
                    origin.as_deref(),
                )
                .await;
        }

        let upstream_url = match target_url(&req) {
            Ok(url) => url,
            Err(error) => {
                return self
                    .write_text_stream(writer, StatusCode::BAD_REQUEST, &error, origin.as_deref())
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
                    .write_text_stream(writer, StatusCode::FORBIDDEN, &error, origin.as_deref())
                    .await
            }
            Err(ProxyFetchError::BadGateway(error)) => {
                return self
                    .write_text_stream(
                        writer,
                        StatusCode::BAD_GATEWAY,
                        &format!("Upstream fetch failed: {error}"),
                        origin.as_deref(),
                    )
                    .await;
            }
        };

        if req.method() == Method::GET && is_hls_response(&response) {
            return self
                .write_hls_playlist(writer, response, origin.as_deref())
                .await;
        }

        let head = streaming_head(&response, origin.as_deref())?;
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
        origin: Option<&str>,
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
            .header("access-control-allow-origin", cors_allow_origin(origin))
            .header("access-control-allow-methods", "GET, HEAD, OPTIONS")
            .header(
                "access-control-allow-headers",
                CORS_ALLOW_HEADERS,
            )
            .header("access-control-allow-private-network", "true")
            .header("cross-origin-resource-policy", "cross-origin")
            .header("timing-allow-origin", "*")
            .header(
                "vary",
                "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
            )
            .header(
                "access-control-expose-headers",
                "accept-ranges, content-length, content-range, content-type, etag, last-modified, x-handled-by",
            )
            .body(())?;
        writer.send_response(response).await?;
        writer.send_data(body).await?;
        writer.finish().await
    }

    async fn fetch_best_innertube_player_response(
        &self,
        video_id: &str,
    ) -> Result<(String, Value, Vec<Value>), String> {
        let mut fallback: Option<(String, Value)> = None;
        let mut attempts = Vec::new();

        for client in innertube_clients() {
            match self
                .fetch_innertube_player_response(video_id, &client)
                .await
            {
                Ok(player_response) => {
                    let status = player_response
                        .pointer("/playabilityStatus/status")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let has_streaming_data = player_response.get("streamingData").is_some();
                    let has_video_streams = player_response_has_streams(&player_response);
                    attempts.push(json!({
                        "client": client.id,
                        "status": if status.is_empty() { Value::Null } else { Value::String(status.clone()) },
                        "hasStreamingData": has_streaming_data,
                        "hasVideoStreams": has_video_streams,
                    }));
                    fallback = Some((format!("innertube_{}", client.id), player_response.clone()));
                    if has_video_streams && !player_response_needs_challenge(&player_response) {
                        return Ok((
                            format!("innertube_{}", client.id),
                            player_response,
                            attempts,
                        ));
                    }
                }
                Err(error) => {
                    attempts.push(json!({
                        "client": client.id,
                        "error": error,
                    }));
                }
            }
        }

        if let Some((resolver, player_response)) = fallback {
            return Ok((resolver, player_response, attempts));
        }

        let summary = attempts
            .iter()
            .map(|attempt| {
                let client = attempt
                    .get("client")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let status = attempt
                    .get("status")
                    .and_then(Value::as_str)
                    .or_else(|| attempt.get("error").and_then(Value::as_str))
                    .unwrap_or("unknown");
                format!("{client}: {status}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("all InnerTube clients failed ({summary})"))
    }

    async fn fetch_innertube_player_response(
        &self,
        video_id: &str,
        client: &InnertubeClient,
    ) -> Result<Value, String> {
        let mut context = json!({ "client": client.context.clone() });
        let visitor_data = if client.id == "android_vr" {
            let visitor_data = self.fetch_youtube_visitor_data(video_id).await?;
            context["client"]["visitorData"] = Value::String(visitor_data.clone());
            Some(visitor_data)
        } else {
            None
        };
        if let Some(third_party) = client.third_party.clone() {
            context["thirdParty"] = third_party;
        }

        let body = json!({
            "videoId": video_id,
            "contentCheckOk": true,
            "racyCheckOk": true,
            "context": context,
            "playbackContext": {
                "contentPlaybackContext": {
                    "html5Preference": "HTML5_PREF_WANTS"
                }
            }
        });
        let endpoint = if client.id == "android_vr" {
            "https://www.youtube.com/youtubei/v1/player?prettyPrint=false".to_string()
        } else {
            format!("https://youtubei.googleapis.com/youtubei/v1/player?key={YOUTUBE_WEB_API_KEY}")
        };
        let mut request = self
            .client
            .post(endpoint)
            .header(reqwest::header::USER_AGENT, client.user_agent)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ORIGIN, "https://www.youtube.com")
            .header(reqwest::header::REFERER, "https://www.youtube.com/");
        if client.id == "android_vr" {
            request = request
                .header("x-youtube-client-name", "28")
                .header("x-youtube-client-version", "1.65.10");
            if let Some(visitor_data) = visitor_data {
                request = request.header("x-goog-visitor-id", visitor_data);
            }
        }
        let response = request
            .json(&body)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("InnerTube HTTP {status}"));
        }
        response
            .json::<Value>()
            .await
            .map_err(|error| error.to_string())
    }

    async fn resolve_ytdlp_frame_source(
        &self,
        source_url: &str,
    ) -> Result<FrameMediaSource, String> {
        if !self.ytdlp.enabled {
            return Err("yt-dlp resolver is disabled".to_string());
        }

        let mut command = Command::new(&self.ytdlp.path);
        command
            .arg("--dump-single-json")
            .arg("--no-playlist")
            .arg("--no-warnings")
            .arg("--skip-download")
            .arg("--no-progress");
        if let Some(extractor_args) = &self.ytdlp.extractor_args {
            command.arg("--extractor-args").arg(extractor_args);
        }
        if let Some(cookies) = &self.ytdlp.cookies {
            command.arg("--cookies").arg(cookies);
        }
        if let Some(cookies_from_browser) = &self.ytdlp.cookies_from_browser {
            command
                .arg("--cookies-from-browser")
                .arg(cookies_from_browser);
        }
        command
            .arg(source_url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = tokio::time::timeout(self.ytdlp.timeout, command.output())
            .await
            .map_err(|_| format!("yt-dlp timed out after {:?}", self.ytdlp.timeout))?
            .map_err(|error| format!("yt-dlp could not start: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "yt-dlp exited with {}{}",
                output.status,
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                }
            ));
        }

        let value = serde_json::from_slice::<Value>(&output.stdout)
            .map_err(|error| format!("yt-dlp JSON parse failed: {error}"))?;
        select_best_ytdlp_frame_media_source(&value)
            .ok_or_else(|| "yt-dlp returned no VP9 WebM video-only format".to_string())
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
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
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
        origin: Option<&str>,
    ) -> HandlerResult<()> {
        let response = Response::builder()
            .status(status)
            .header("cache-control", "no-store")
            .header("x-handled-by", "av-ingest-proxy")
            .header("access-control-allow-origin", cors_allow_origin(origin))
            .header("access-control-allow-methods", "GET, HEAD, OPTIONS")
            .header("access-control-allow-headers", CORS_ALLOW_HEADERS)
            .header("access-control-allow-private-network", "true")
            .header("cross-origin-resource-policy", "cross-origin")
            .header("timing-allow-origin", "*")
            .header(
                "vary",
                "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
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
        origin: Option<&str>,
    ) -> HandlerResult<()> {
        let response = Response::builder()
            .status(status)
            .header("content-type", "text/plain; charset=utf-8")
            .header("cache-control", "no-store")
            .header("x-handled-by", "av-ingest-proxy")
            .header("access-control-allow-origin", cors_allow_origin(origin))
            .header("access-control-allow-methods", "GET, HEAD, OPTIONS")
            .header("access-control-allow-headers", CORS_ALLOW_HEADERS)
            .header("access-control-allow-private-network", "true")
            .header("cross-origin-resource-policy", "cross-origin")
            .header("timing-allow-origin", "*")
            .header(
                "vary",
                "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
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

fn extract_json_string_field(text: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let mut remaining = text;
    while let Some(index) = remaining.find(&needle) {
        let after_field = &remaining[index + needle.len()..];
        let Some(colon_index) = after_field.find(':') else {
            return None;
        };
        let value = after_field[colon_index + 1..].trim_start();
        let Some(value) = value.strip_prefix('"') else {
            remaining = after_field;
            continue;
        };
        let mut escaped = false;
        for (end, ch) in value.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => return serde_json::from_str(&format!("\"{}\"", &value[..end])).ok(),
                _ => {}
            }
        }
        remaining = after_field;
    }
    None
}

struct InnertubeClient {
    id: &'static str,
    user_agent: &'static str,
    context: Value,
    third_party: Option<Value>,
}

fn innertube_clients() -> Vec<InnertubeClient> {
    vec![
        InnertubeClient {
            id: "android_vr",
            user_agent: ANDROID_VR_USER_AGENT,
            context: json!({
                "clientName": "ANDROID_VR",
                "clientVersion": "1.65.10",
                "deviceMake": "Oculus",
                "deviceModel": "Quest 3",
                "androidSdkVersion": 32,
                "userAgent": ANDROID_VR_USER_AGENT,
                "osName": "Android",
                "osVersion": "12L",
                "hl": "en",
                "gl": "US"
            }),
            third_party: None,
        },
        InnertubeClient {
            id: "android",
            user_agent: ANDROID_USER_AGENT,
            context: json!({
                "clientName": "ANDROID",
                "clientVersion": "21.03.36",
                "androidSdkVersion": 36,
                "osName": "Android",
                "osVersion": "13",
                "platform": "MOBILE",
                "clientFormFactor": "SMALL_FORM_FACTOR",
                "userAgent": ANDROID_USER_AGENT,
                "hl": "en",
                "gl": "US"
            }),
            third_party: None,
        },
        InnertubeClient {
            id: "android_embedded",
            user_agent: ANDROID_USER_AGENT,
            context: json!({
                "clientName": "ANDROID",
                "clientVersion": "21.03.36",
                "androidSdkVersion": 36,
                "osName": "Android",
                "osVersion": "13",
                "platform": "MOBILE",
                "clientFormFactor": "SMALL_FORM_FACTOR",
                "clientScreen": "EMBED",
                "userAgent": ANDROID_USER_AGENT,
                "hl": "en",
                "gl": "US"
            }),
            third_party: Some(json!({"embedUrl": "https://www.youtube.com"})),
        },
        InnertubeClient {
            id: "ios",
            user_agent: IOS_USER_AGENT,
            context: json!({
                "clientName": "iOS",
                "clientVersion": "20.11.6",
                "deviceMake": "Apple",
                "deviceModel": "iPhone10,4",
                "osName": "iOS",
                "osVersion": "16.7.7.20H330",
                "platform": "MOBILE",
                "userAgent": IOS_USER_AGENT,
                "hl": "en",
                "gl": "US"
            }),
            third_party: None,
        },
        InnertubeClient {
            id: "ios_embedded",
            user_agent: IOS_USER_AGENT,
            context: json!({
                "clientName": "iOS",
                "clientVersion": "20.11.6",
                "deviceMake": "Apple",
                "deviceModel": "iPhone10,4",
                "osName": "iOS",
                "osVersion": "16.7.7.20H330",
                "platform": "MOBILE",
                "clientScreen": "EMBED",
                "userAgent": IOS_USER_AGENT,
                "hl": "en",
                "gl": "US"
            }),
            third_party: Some(json!({"embedUrl": "https://www.youtube.com"})),
        },
    ]
}

#[async_trait]
impl Router for MediaProxy {
    async fn route(&self, req: Request<()>) -> HandlerResult<HandlerResponse> {
        match req.uri().path() {
            "/healthz" => Ok(self.healthz()),
            "/resolve" => Ok(self.resolve_source(req).await),
            "/frame" => Ok(self.frame_image(req).await),
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

const CORS_ALLOW_HEADERS: &str =
    "Origin, Accept, Range, Content-Type, If-Range, If-None-Match, If-Modified-Since, Cache-Control, Pragma";

fn request_origin(req: &Request<()>) -> Option<String> {
    req.headers()
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn cors_allow_origin(origin: Option<&str>) -> &str {
    origin.unwrap_or("*")
}

fn streaming_head(
    response: &reqwest::Response,
    origin: Option<&str>,
) -> HandlerResult<Response<()>> {
    let mut builder = Response::builder().status(status_from_reqwest(response.status())?);
    for (name, value) in response.headers() {
        if should_forward_response_header(name.as_str()) {
            builder = builder.header(name.as_str(), value);
        }
    }
    builder = builder
        .header("cache-control", "no-store")
        .header("x-handled-by", "av-ingest-proxy")
        .header("access-control-allow-origin", cors_allow_origin(origin))
        .header("access-control-allow-methods", "GET, HEAD, OPTIONS")
        .header(
            "access-control-allow-headers",
            CORS_ALLOW_HEADERS,
        )
        .header("access-control-allow-private-network", "true")
        .header("cross-origin-resource-policy", "cross-origin")
        .header("timing-allow-origin", "*")
        .header(
            "vary",
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        )
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

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn target_url(req: &Request<()>) -> Result<Url, String> {
    let query = req.uri().query().unwrap_or_default();
    let value =
        query_param(query, "url").ok_or_else(|| "Missing url query parameter".to_string())?;
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

fn is_youtube_host(hostname: &str) -> bool {
    let host = hostname.to_ascii_lowercase();
    host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com")
        || host == "youtu.be"
        || host.ends_with(".youtu.be")
}

fn extract_youtube_id(url: &Url) -> Option<String> {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if host == "youtu.be" || host.ends_with(".youtu.be") {
        return url
            .path_segments()
            .and_then(|mut segments| segments.next())
            .and_then(clean_youtube_id);
    }
    if let Some((_, value)) = url.query_pairs().find(|(key, _)| key == "v") {
        return clean_youtube_id(&value);
    }
    let segments = url.path_segments()?.collect::<Vec<_>>();
    for prefix in ["shorts", "embed", "live", "v"] {
        if segments.first().is_some_and(|segment| *segment == prefix) {
            return segments.get(1).and_then(|value| clean_youtube_id(value));
        }
    }
    None
}

fn clean_youtube_id(value: &str) -> Option<String> {
    let id = value
        .trim()
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .collect::<String>();
    (id.len() == 11).then_some(id)
}

fn query_resolve_mode(query: &str) -> Option<ResolveMode> {
    query_param(query, "mode")
        .or_else(|| query_param(query, "resolve_mode"))
        .and_then(|value| percent_decode(value).ok())
        .and_then(|value| ResolveMode::parse(&value).ok())
}

fn player_response_has_formats_for_mode(player_response: &Value, mode: ResolveMode) -> bool {
    match mode {
        ResolveMode::Full => player_response_has_streams(player_response),
        ResolveMode::Transcribe => iter_player_formats(player_response).any(format_has_audio),
    }
}

fn player_response_has_streams(player_response: &Value) -> bool {
    iter_player_formats(player_response).any(|format| {
        format
            .get("mimeType")
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.starts_with("video/"))
    })
}

fn player_response_needs_challenge(player_response: &Value) -> bool {
    iter_player_formats(player_response).any(format_needs_challenge)
}

fn iter_player_formats(player_response: &Value) -> impl Iterator<Item = &Value> {
    player_response
        .pointer("/streamingData/formats")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            player_response
                .pointer("/streamingData/adaptiveFormats")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
}

fn apply_transcribe_resolve_mode(player_response: &mut Value) {
    let Some(streaming_data) = player_response
        .get_mut("streamingData")
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    let audio_formats = streaming_data
        .get("adaptiveFormats")
        .and_then(Value::as_array)
        .map(|formats| {
            formats
                .iter()
                .filter(|format| is_audio_only_format(format))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if !audio_formats.is_empty() {
        streaming_data.insert("formats".to_string(), Value::Array(Vec::new()));
        streaming_data.insert("adaptiveFormats".to_string(), Value::Array(audio_formats));
        return;
    }

    let smallest_muxed = streaming_data
        .get("formats")
        .and_then(Value::as_array)
        .and_then(|formats| {
            formats
                .iter()
                .filter(|format| format_has_audio(format) && format_has_video(format))
                .min_by_key(|format| transcribe_fallback_score(format))
                .cloned()
        });

    streaming_data.insert(
        "formats".to_string(),
        Value::Array(smallest_muxed.into_iter().collect()),
    );
    streaming_data.insert("adaptiveFormats".to_string(), Value::Array(Vec::new()));
}

fn is_audio_only_format(format: &Value) -> bool {
    format_has_audio(format) && !format_has_video(format)
}

fn format_has_audio(format: &Value) -> bool {
    let mime = format
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    mime.starts_with("audio/") || format.get("audioQuality").is_some()
}

fn format_has_video(format: &Value) -> bool {
    let mime = format
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    mime.starts_with("video/") || format.get("width").is_some() || format.get("height").is_some()
}

fn transcribe_fallback_score(format: &Value) -> (u64, u64, u64) {
    let pixels = format_u64(format, "width")
        .zip(format_u64(format, "height"))
        .map(|(width, height)| width.saturating_mul(height))
        .unwrap_or(u64::MAX);
    let bitrate = format_u64(format, "bitrate").unwrap_or(u64::MAX);
    let content_length = format_u64(format, "contentLength").unwrap_or(u64::MAX);
    (pixels, bitrate, content_length)
}

fn select_transcribe_media_source(
    player_response: &Value,
    resolver: &str,
) -> Option<TranscribeMediaSource> {
    iter_player_formats(player_response)
        .filter(|format| format_has_audio(format))
        .filter_map(|format| {
            let url = format.get("url").and_then(Value::as_str)?;
            let url = Url::parse(url).ok()?;
            Some((
                transcribe_media_score(format),
                TranscribeMediaSource {
                    url,
                    resolver: resolver.to_string(),
                    itag: format_u64(format, "itag"),
                    mime_type: format
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                },
            ))
        })
        .min_by_key(|(score, _source)| *score)
        .map(|(_score, source)| source)
}

fn transcribe_media_score(format: &Value) -> (u8, u8, u64, u64, u64) {
    let audio_only = is_audio_only_format(format);
    let pixels = format_u64(format, "width")
        .zip(format_u64(format, "height"))
        .map(|(width, height)| width.saturating_mul(height))
        .unwrap_or(0);
    let content_length = format_u64(format, "contentLength").unwrap_or(u64::MAX);
    let bitrate = format_u64(format, "bitrate").unwrap_or(u64::MAX);
    (
        (!audio_only) as u8,
        transcribe_codec_score(format),
        pixels,
        content_length,
        bitrate,
    )
}

fn transcribe_codec_score(format: &Value) -> u8 {
    let mime = format
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if mime.contains("webm") && mime.contains("opus") {
        0
    } else if mime.contains("mp4a.40.2") {
        1
    } else if mime.contains("mp4a") {
        2
    } else {
        3
    }
}

fn format_u64(format: &Value, key: &str) -> Option<u64> {
    format
        .get(key)
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

fn select_best_innertube_frame_media_source(
    player_response: &Value,
    resolver: &str,
) -> Option<FrameMediaSource> {
    iter_player_formats(player_response)
        .filter_map(|format| {
            if !is_native_frame_format(format) {
                return None;
            }
            let url = format.get("url").and_then(Value::as_str)?;
            let parsed = Url::parse(url).ok()?;
            Some((
                frame_format_score(format),
                FrameMediaSource {
                    url: parsed,
                    headers: Vec::new(),
                    resolver: resolver.to_string(),
                },
            ))
        })
        .max_by_key(|(score, _source)| *score)
        .map(|(_score, source)| source)
}

fn is_native_frame_format(format: &Value) -> bool {
    let mime = format
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    mime.starts_with("video/")
        && mime.contains("webm")
        && (mime.contains("vp9") || mime.contains("vp09"))
}

fn frame_format_score(format: &Value) -> i64 {
    let width = format
        .get("width")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let height = format
        .get("height")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let bitrate = format
        .get("bitrate")
        .or_else(|| format.get("averageBitrate"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);

    width
        .saturating_mul(height)
        .saturating_add((bitrate / 1000).min(100_000))
}

fn format_needs_challenge(format: &Value) -> bool {
    if format.get("signatureCipher").is_some() || format.get("cipher").is_some() {
        return true;
    }
    let Some(url) = format.get("url").and_then(Value::as_str) else {
        return false;
    };
    Url::parse(url)
        .ok()
        .is_some_and(|url| url.query_pairs().any(|(key, _)| key == "n"))
}

fn select_best_ytdlp_frame_media_source(root: &Value) -> Option<FrameMediaSource> {
    let candidates: Vec<(&Value, &Value)> = if root.get("formats").is_some() {
        vec![(root, root)]
    } else {
        root.get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|entry| (entry, entry))
            .collect()
    };

    candidates
        .into_iter()
        .flat_map(|(video, entry)| {
            entry
                .get("formats")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |format| {
                    if !is_ytdlp_native_frame_format(format) {
                        return None;
                    }
                    let url = format.get("url").and_then(Value::as_str)?;
                    let parsed = Url::parse(url).ok()?;
                    Some((
                        ytdlp_frame_format_score(format),
                        FrameMediaSource {
                            url: parsed,
                            headers: collect_ytdlp_http_headers(video, format),
                            resolver: "yt-dlp".to_string(),
                        },
                    ))
                })
        })
        .max_by_key(|(score, _source)| *score)
        .map(|(_score, source)| source)
}

fn is_ytdlp_native_frame_format(format: &Value) -> bool {
    let protocol = format
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if !protocol.is_empty() && protocol != "https" && protocol != "http" {
        return false;
    }

    let ext = format
        .get("ext")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let vcodec = format
        .get("vcodec")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let acodec = format
        .get("acodec")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let url = format.get("url").and_then(Value::as_str).unwrap_or("");

    !url.is_empty()
        && ext == "webm"
        && (vcodec.contains("vp9") || vcodec.contains("vp09"))
        && (acodec.is_empty() || acodec == "none")
}

fn ytdlp_frame_format_score(format: &Value) -> i64 {
    let width = format
        .get("width")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let height = format
        .get("height")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let bitrate = format
        .get("tbr")
        .or_else(|| format.get("vbr"))
        .or_else(|| format.get("abr"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .max(0.0)
        .round() as i64;

    width
        .saturating_mul(height)
        .saturating_add(bitrate.min(100_000))
}

fn collect_ytdlp_http_headers(video: &Value, format: &Value) -> Vec<(String, String)> {
    let mut headers = BTreeMap::<String, (String, String)>::new();
    merge_ytdlp_http_headers(&mut headers, video.get("http_headers"));
    merge_ytdlp_http_headers(&mut headers, format.get("http_headers"));
    headers.into_values().collect()
}

fn merge_ytdlp_http_headers(
    headers: &mut BTreeMap<String, (String, String)>,
    value: Option<&Value>,
) {
    let Some(object) = value.and_then(Value::as_object) else {
        return;
    };
    for (name, value) in object {
        let Some(value) = value.as_str() else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        headers.insert(name.to_ascii_lowercase(), (name.clone(), value.to_string()));
    }
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
        ("access-control-allow-origin".to_string(), "*".to_string()),
        (
            "access-control-allow-methods".to_string(),
            "GET, HEAD, OPTIONS".to_string(),
        ),
        (
            "access-control-allow-headers".to_string(),
            CORS_ALLOW_HEADERS.to_string(),
        ),
        (
            "access-control-allow-private-network".to_string(),
            "true".to_string(),
        ),
        (
            "cross-origin-resource-policy".to_string(),
            "cross-origin".to_string(),
        ),
        ("timing-allow-origin".to_string(), "*".to_string()),
        (
            "vary".to_string(),
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_string(),
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

fn env_nonempty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_u16(name: &str, default: u16) -> Result<u16> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u16>()
            .with_context(|| format!("failed to parse {name}={value} as u16")),
        Err(_) => Ok(default),
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("failed to parse {name}={value} as u64")),
        Err(_) => Ok(default),
    }
}

fn frame_timestamp_us(query: &str) -> Result<u64, String> {
    for name in ["ts_us", "time_us", "timestamp_us"] {
        if let Some(value) = query_param(query, name) {
            return value
                .parse::<u64>()
                .map_err(|_| format!("Invalid {name} query parameter"));
        }
    }
    if let Some(value) = query_param(query, "t") {
        let seconds = value
            .parse::<f64>()
            .map_err(|_| "Invalid t query parameter".to_string())?;
        if !seconds.is_finite() || seconds < 0.0 {
            return Err("Invalid t query parameter".to_string());
        }
        return Ok((seconds * 1_000_000.0).round() as u64);
    }
    Err("Missing ts_us query parameter".to_string())
}

type PlainHttpBody = BoxBody<Bytes, Infallible>;

struct PlainHttpStreamWriter {
    response_tx: Option<oneshot::Sender<Result<Response<()>, ServerError>>>,
    data_tx: Option<mpsc::Sender<Bytes>>,
}

impl PlainHttpStreamWriter {
    fn new(
        response_tx: oneshot::Sender<Result<Response<()>, ServerError>>,
        data_tx: mpsc::Sender<Bytes>,
    ) -> Self {
        Self {
            response_tx: Some(response_tx),
            data_tx: Some(data_tx),
        }
    }
}

#[async_trait]
impl StreamWriter for PlainHttpStreamWriter {
    async fn send_response(&mut self, response: Response<()>) -> Result<(), ServerError> {
        let tx = self
            .response_tx
            .take()
            .ok_or_else(|| ServerError::Config("stream response already sent".into()))?;
        tx.send(Ok(response))
            .map_err(|_| ServerError::Config("failed to send stream response head".into()))
    }

    async fn send_data(&mut self, data: Bytes) -> Result<(), ServerError> {
        let tx = self
            .data_tx
            .as_ref()
            .ok_or_else(|| ServerError::Config("stream already finished".into()))?;
        tx.send(data)
            .await
            .map_err(|_| ServerError::Config("failed to send stream body chunk".into()))
    }

    async fn finish(&mut self) -> Result<(), ServerError> {
        self.data_tx.take();
        Ok(())
    }
}

async fn run_plain_http_server(port: u16, router: Arc<dyn Router>) -> Result<()> {
    let addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind local HTTP av-ingest proxy at {addr}"))?;

    info!("HTTP/1.1 local dev server listening at {}", addr);
    info!(
        port = port,
        h3 = false,
        tls = false,
        "av ingest proxy ready"
    );

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutting down av ingest proxy");
                break;
            }
            accept_result = listener.accept() => {
                let (stream, _) = match accept_result {
                    Ok(value) => value,
                    Err(error) => {
                        warn!("local HTTP accept failed: {}", error);
                        continue;
                    }
                };
                let router = Arc::clone(&router);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        handle_plain_http_request(request, Arc::clone(&router))
                    });
                    if let Err(error) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        warn!("local HTTP connection failed: {}", error);
                    }
                });
            }
        }
    }

    Ok(())
}

async fn handle_plain_http_request(
    req: Request<Incoming>,
    router: Arc<dyn Router>,
) -> Result<Response<PlainHttpBody>, Infallible> {
    let (parts, _) = req.into_parts();
    let req = Request::from_parts(parts, ());

    if router.is_streaming(req.uri().path()) {
        return Ok(handle_plain_http_stream(req, router).await);
    }

    let response = match router.route(req).await {
        Ok(response) => response,
        Err(error) => HandlerResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: Some(Bytes::from(format!("Internal server error: {error}\n"))),
            content_type: Some("text/plain; charset=utf-8".to_string()),
            headers: base_response_headers(),
            etag: None,
        },
    };
    Ok(handler_response_to_plain_http(response))
}

async fn handle_plain_http_stream(
    req: Request<()>,
    router: Arc<dyn Router>,
) -> Response<PlainHttpBody> {
    let (response_tx, response_rx) = oneshot::channel();
    let (data_tx, data_rx) = mpsc::channel::<Bytes>(32);
    let writer = PlainHttpStreamWriter::new(response_tx, data_tx);

    tokio::spawn(async move {
        let _ = router.route_stream(req, Box::new(writer)).await;
    });

    let response = match response_rx.await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return handler_response_to_plain_http(HandlerResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: Some(Bytes::from(format!("Internal server error: {error}\n"))),
                content_type: Some("text/plain; charset=utf-8".to_string()),
                headers: base_response_headers(),
                etag: None,
            });
        }
        Err(_) => {
            return handler_response_to_plain_http(HandlerResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: Some(Bytes::from_static(b"Internal server error\n")),
                content_type: Some("text/plain; charset=utf-8".to_string()),
                headers: base_response_headers(),
                etag: None,
            });
        }
    };

    let (parts, ()) = response.into_parts();
    let body = stream::unfold(data_rx, |mut rx| async {
        rx.recv()
            .await
            .map(|chunk| (Ok::<Frame<Bytes>, Infallible>(Frame::data(chunk)), rx))
    });
    Response::from_parts(parts, BodyExt::boxed(StreamBody::new(body)))
}

fn handler_response_to_plain_http(response: HandlerResponse) -> Response<PlainHttpBody> {
    let mut builder = Response::builder().status(response.status);
    if let Some(content_type) = response.content_type {
        builder = builder.header("content-type", content_type);
    }
    for (name, value) in response.headers {
        builder = builder.header(name, value);
    }
    if let Some(etag) = response.etag {
        builder = builder.header("etag", etag);
    }
    builder
        .body(Full::new(response.body.unwrap_or_default()).boxed())
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::from_static(b"Internal server error\n")).boxed())
                .expect("static fallback response is valid")
        })
}

pub async fn run_from_env() -> Result<()> {
    let _ = dotenvy::dotenv();
    let config = AppConfig::from_env()?;
    run(config).await
}

async fn run(config: AppConfig) -> Result<()> {
    if config.local_http {
        let router = Arc::new(MediaProxy::new(
            config.user_agent.clone(),
            config.ytdlp.clone(),
            config.resolve_mode,
        )?);
        return run_plain_http_server(config.port, router).await;
    }

    let (cert, key) = config.tls_base64()?;
    let router = Box::new(MediaProxy::new(
        config.user_agent.clone(),
        config.ytdlp.clone(),
        config.resolve_mode,
    )?);
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

    #[test]
    fn transcribe_resolve_mode_keeps_audio_only_formats() {
        let mut value = json!({
            "streamingData": {
                "formats": [
                    {
                        "itag": 18,
                        "mimeType": "video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"",
                        "width": 640,
                        "height": 360,
                        "audioQuality": "AUDIO_QUALITY_LOW"
                    }
                ],
                "adaptiveFormats": [
                    {
                        "itag": 140,
                        "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                        "audioQuality": "AUDIO_QUALITY_MEDIUM",
                        "bitrate": 128000
                    },
                    {
                        "itag": 248,
                        "mimeType": "video/webm; codecs=\"vp9\"",
                        "width": 1920,
                        "height": 1080
                    }
                ]
            }
        });

        apply_transcribe_resolve_mode(&mut value);

        let formats = value
            .pointer("/streamingData/formats")
            .and_then(Value::as_array)
            .unwrap();
        let adaptive = value
            .pointer("/streamingData/adaptiveFormats")
            .and_then(Value::as_array)
            .unwrap();
        assert!(formats.is_empty());
        assert_eq!(adaptive.len(), 1);
        assert_eq!(adaptive[0].get("itag").and_then(Value::as_i64), Some(140));
    }

    #[test]
    fn transcribe_media_source_prefers_streaming_safe_opus() {
        let value = json!({
            "streamingData": {
                "adaptiveFormats": [
                    {
                        "itag": 139,
                        "url": "https://rr1---sn.googlevideo.com/videoplayback?itag=139",
                        "mimeType": "audio/mp4; codecs=\"mp4a.40.5\"",
                        "audioQuality": "AUDIO_QUALITY_LOW",
                        "contentLength": "1000",
                        "bitrate": 48000
                    },
                    {
                        "itag": 251,
                        "url": "https://rr1---sn.googlevideo.com/videoplayback?itag=251",
                        "mimeType": "audio/webm; codecs=\"opus\"",
                        "audioQuality": "AUDIO_QUALITY_MEDIUM",
                        "contentLength": "2000",
                        "bitrate": 128000
                    }
                ]
            }
        });

        let source = select_transcribe_media_source(&value, "test").unwrap();
        assert_eq!(source.itag, Some(251));
    }

    #[test]
    fn transcribe_resolve_mode_falls_back_to_smallest_muxed_video() {
        let mut value = json!({
            "streamingData": {
                "formats": [
                    {
                        "itag": 22,
                        "mimeType": "video/mp4; codecs=\"avc1.64001F, mp4a.40.2\"",
                        "width": 1280,
                        "height": 720,
                        "audioQuality": "AUDIO_QUALITY_MEDIUM",
                        "bitrate": 1500000
                    },
                    {
                        "itag": 17,
                        "mimeType": "video/3gpp; codecs=\"mp4v.20.3, mp4a.40.2\"",
                        "width": 176,
                        "height": 144,
                        "audioQuality": "AUDIO_QUALITY_LOW",
                        "bitrate": 50000
                    }
                ],
                "adaptiveFormats": [
                    {
                        "itag": 248,
                        "mimeType": "video/webm; codecs=\"vp9\"",
                        "width": 1920,
                        "height": 1080
                    }
                ]
            }
        });

        apply_transcribe_resolve_mode(&mut value);

        let formats = value
            .pointer("/streamingData/formats")
            .and_then(Value::as_array)
            .unwrap();
        let adaptive = value
            .pointer("/streamingData/adaptiveFormats")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].get("itag").and_then(Value::as_i64), Some(17));
        assert!(adaptive.is_empty());
    }

    #[test]
    fn selects_ytdlp_vp9_webm_and_merges_format_headers() {
        let value = json!({
            "http_headers": {
                "User-Agent": "global-agent",
                "Accept-Language": "en-US"
            },
            "formats": [
                {
                    "url": "https://rr1---sn.googlevideo.com/videoplayback?itag=137",
                    "ext": "mp4",
                    "vcodec": "avc1.640028",
                    "acodec": "none",
                    "width": 1920,
                    "height": 1080
                },
                {
                    "url": "https://rr1---sn.googlevideo.com/videoplayback?itag=248",
                    "ext": "webm",
                    "vcodec": "vp9",
                    "acodec": "none",
                    "width": 1920,
                    "height": 1080,
                    "http_headers": {
                        "User-Agent": "format-agent",
                        "Referer": "https://www.youtube.com/"
                    }
                }
            ]
        });

        let selected = select_best_ytdlp_frame_media_source(&value).unwrap();
        assert_eq!(
            selected
                .url
                .query_pairs()
                .find(|(key, _)| key == "itag")
                .unwrap()
                .1,
            "248"
        );
        assert!(selected
            .headers
            .iter()
            .any(|(name, value)| name == "User-Agent" && value == "format-agent"));
        assert!(selected
            .headers
            .iter()
            .any(|(name, value)| name == "Accept-Language" && value == "en-US"));
        assert!(selected
            .headers
            .iter()
            .any(|(name, value)| name == "Referer" && value == "https://www.youtube.com/"));
    }
}
