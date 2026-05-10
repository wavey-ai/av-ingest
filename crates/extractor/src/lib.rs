use serde::Serialize;
use serde_json::Value;
use url::Url;
use wasm_bindgen::prelude::*;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceInfo {
    provider: String,
    canonical_url: String,
    id: Option<String>,
    direct_media: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserVideoFormat {
    itag: Option<u64>,
    label: String,
    url: String,
    mime_type: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
    fps: Option<u64>,
    has_audio: bool,
    source: String,
}

#[wasm_bindgen(js_name = parseSourceUrl)]
pub fn parse_source_url(input: &str) -> Result<JsValue, JsValue> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(js_error("Enter a URL."));
    }

    if let Some(id) = extract_youtube_id(trimmed) {
        return to_js(&SourceInfo {
            provider: "youtube".to_string(),
            canonical_url: format!("https://www.youtube.com/watch?v={id}"),
            id: Some(id),
            direct_media: false,
        });
    }

    let url = Url::parse(trimmed).map_err(|error| js_error(format!("Invalid URL: {error}")))?;
    let host = normalized_host(&url);
    let path = url.path().to_ascii_lowercase();
    let direct_media = matches!(
        path.rsplit('.').next(),
        Some("mp4" | "m4v" | "mov" | "webm" | "m3u8" | "mpd")
    );
    let provider = if host == "soundcloud.com" || host.ends_with(".soundcloud.com") {
        "soundcloud"
    } else if direct_media {
        "direct"
    } else {
        "unsupported"
    };

    to_js(&SourceInfo {
        provider: provider.to_string(),
        canonical_url: url.to_string(),
        id: None,
        direct_media,
    })
}

#[wasm_bindgen(js_name = selectBrowserVideoFormats)]
pub fn select_browser_video_formats(player_response_json: &str) -> Result<JsValue, JsValue> {
    let root: Value = serde_json::from_str(player_response_json)
        .map_err(|error| js_error(format!("Could not parse YouTube player response: {error}")))?;
    let mut formats = Vec::new();
    collect_formats(&root, "formats", &mut formats);
    collect_formats(&root, "adaptiveFormats", &mut formats);

    formats.sort_by(|a, b| format_score(b).cmp(&format_score(a)));
    formats.dedup_by(|a, b| a.url == b.url);
    if formats.is_empty() {
        return Err(js_error(
            "No browser-playable YouTube URL is available yet. This video requires the YouTube player JS n/signature challenge solver.",
        ));
    }
    to_js(&formats)
}

fn collect_formats(root: &Value, key: &str, out: &mut Vec<BrowserVideoFormat>) {
    let Some(items) = root
        .get("streamingData")
        .and_then(|streaming| streaming.get(key))
        .and_then(Value::as_array)
    else {
        return;
    };

    for item in items {
        let mime_type = item
            .get("mimeType")
            .and_then(Value::as_str)
            .map(str::to_string);
        let is_video = mime_type
            .as_deref()
            .is_some_and(|mime| mime.starts_with("video/"));
        if !is_video {
            continue;
        }
        let Some(url) = item.get("url").and_then(Value::as_str) else {
            continue;
        };
        if url_has_n_challenge(url) {
            continue;
        }
        let width = item.get("width").and_then(Value::as_u64);
        let height = item.get("height").and_then(Value::as_u64);
        let fps = item.get("fps").and_then(Value::as_u64);
        let itag = item.get("itag").and_then(Value::as_u64);
        let has_audio = item.get("audioQuality").is_some()
            || item
                .get("mimeType")
                .and_then(Value::as_str)
                .is_some_and(|mime| mime.contains("mp4a") || mime.contains("opus"));
        let codec = mime_type
            .as_deref()
            .and_then(|mime| mime.split("codecs=").nth(1))
            .unwrap_or("")
            .trim_matches('"');
        let label = match (height, fps, has_audio) {
            (Some(height), Some(fps), true) => format!("{height}p{fps} + audio {codec}"),
            (Some(height), Some(fps), false) => format!("{height}p{fps} video {codec}"),
            (Some(height), None, true) => format!("{height}p + audio {codec}"),
            (Some(height), None, false) => format!("{height}p video {codec}"),
            _ => format!("video {codec}"),
        };
        out.push(BrowserVideoFormat {
            itag,
            label,
            url: url.to_string(),
            mime_type,
            width,
            height,
            fps,
            has_audio,
            source: key.to_string(),
        });
    }
}

fn url_has_n_challenge(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .is_some_and(|url| url.query_pairs().any(|(key, _)| key == "n"))
}

fn format_score(format: &BrowserVideoFormat) -> u64 {
    let height = format.height.unwrap_or(0).min(2160);
    let fps = format.fps.unwrap_or(30).min(60);
    let codec_bonus = format
        .mime_type
        .as_deref()
        .map(|mime| {
            if mime.contains("avc1") {
                500
            } else if mime.contains("vp9") || mime.contains("av01") {
                300
            } else {
                0
            }
        })
        .unwrap_or(0);
    let audio_bonus = if format.has_audio { 200 } else { 0 };
    height * 10 + fps + codec_bonus + audio_bonus
}

fn extract_youtube_id(input: &str) -> Option<String> {
    if is_clean_youtube_id(input) {
        return Some(input.to_string());
    }
    let url = Url::parse(input).ok()?;
    let host = normalized_host(&url);
    if host == "youtu.be" || host.ends_with(".youtu.be") {
        return url
            .path_segments()
            .and_then(|mut segments| segments.next())
            .and_then(clean_youtube_id);
    }
    let is_youtube_host = host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com");
    if !is_youtube_host {
        return None;
    }
    if let Some(id) = url
        .query_pairs()
        .find(|(key, _)| key == "v")
        .and_then(|(_, value)| clean_youtube_id(&value))
    {
        return Some(id);
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
    is_clean_youtube_id(&id).then_some(id)
}

fn is_clean_youtube_id(value: &str) -> bool {
    value.len() == 11
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn normalized_host(url: &Url) -> String {
    url.host_str()
        .unwrap_or_default()
        .trim_start_matches("www.")
        .to_ascii_lowercase()
}

fn to_js<T: Serialize + ?Sized>(value: &T) -> Result<JsValue, JsValue> {
    let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    value.serialize(&serializer).map_err(js_error)
}

fn js_error(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_common_youtube_ids() {
        assert_eq!(
            extract_youtube_id("https://www.youtube.com/watch?v=jNQXAC9IVRw").as_deref(),
            Some("jNQXAC9IVRw")
        );
        assert_eq!(
            extract_youtube_id("https://youtu.be/jNQXAC9IVRw?t=1").as_deref(),
            Some("jNQXAC9IVRw")
        );
        assert_eq!(
            extract_youtube_id("https://www.youtube.com/shorts/jNQXAC9IVRw").as_deref(),
            Some("jNQXAC9IVRw")
        );
    }

    #[test]
    fn detects_youtube_n_challenge_urls() {
        assert!(url_has_n_challenge(
            "https://rr1---sn.googlevideo.com/videoplayback?itag=18&n=abc123"
        ));
        assert!(!url_has_n_challenge(
            "https://cdn.example.com/video.mp4?token=abc123"
        ));
    }
}
