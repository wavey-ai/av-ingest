# av-ingest

Local Rust proxy for resolving public media URLs, proxying media bytes, and
extracting source frames by timestamp.

The main consumer is a browser app that needs safe preview playback plus an exact
source frame for rendering. Browser playback can use a lower-resolution compatible
stream; `/frame` can use the highest-resolution source stream.

## Requirements

```bash
brew install libvpx pkg-config yt-dlp
```

You also need Rust/Cargo.

## Run locally

```bash
AV_INGEST_PROXY_LOCAL_HTTP=1 \
AV_INGEST_PROXY_PORT=8444 \
cargo run -p av-ingest-proxy
```

Proxy base URL:

```text
http://127.0.0.1:8444
```

Point local clients at that URL. For example, bitneedle-presser uses:

```text
BITNEEDLE_AV_INGEST_PROXY_BASE=http://127.0.0.1:8444
```

## Endpoints

```text
GET      /healthz
GET      /resolve?url=...
GET|HEAD /proxy?url=...
GET      /frame?url=...&ts_us=...
```

`/resolve` resolves a supported source URL and returns stream metadata.

`/proxy` forwards media bytes and supports range requests. The browser uses this
for playback and seeking.

`/frame` extracts a still image from the source stream at a microsecond
timestamp. It selects a high-resolution WebM/VP9 stream, seeks by WebM cues,
decodes with `libvpx`, and returns PNG.

For YouTube frame extraction, the proxy first asks `yt-dlp` for current format
URLs and uses the selected format's `http_headers` for every native range fetch.
If that fails, it falls back to the built-in InnerTube resolver.

## Smoke tests

```bash
curl -fsS http://127.0.0.1:8444/healthz
```

```bash
source_url="$(node -e 'console.log(encodeURIComponent(process.argv[1]))' \
  'https://www.youtube.com/watch?v=VIDEO_ID')"

curl -fsS \
  "http://127.0.0.1:8444/resolve?url=${source_url}" \
  -o /tmp/resolve.json
```

```bash
media_url="$(node -e 'console.log(encodeURIComponent(process.argv[1]))' \
  'https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4')"

curl -fsS -H 'Range: bytes=0-1023' \
  "http://127.0.0.1:8444/proxy?url=${media_url}" \
  -o /tmp/flower.bin
```

```bash
source_url="$(node -e 'console.log(encodeURIComponent(process.argv[1]))' \
  'https://www.youtube.com/watch?v=VIDEO_ID')"

curl -fsS \
  "http://127.0.0.1:8444/frame?url=${source_url}&ts_us=114000000" \
  -o /tmp/frame.png
```

## Frame extraction

Native path:

1. Resolve source streams with `yt-dlp`, including per-format HTTP headers.
2. Pick the best WebM/VP9 video stream.
3. Fetch enough bytes to parse WebM metadata and cues.
4. Range-fetch the cluster around `ts_us`.
5. Decode VP9 with `libvpx`.
6. Return PNG.

Useful environment variables:

```bash
AV_INGEST_PROXY_YTDLP_ENABLED=1
AV_INGEST_PROXY_YTDLP_PATH=yt-dlp
AV_INGEST_PROXY_YTDLP_TIMEOUT_SECS=45
AV_INGEST_PROXY_YTDLP_DOWNLOAD_TIMEOUT_SECS=21600
AV_INGEST_PROXY_YTDLP_EXTRACTOR_ARGS='youtube:player_client=mweb'
AV_INGEST_PROXY_YTDLP_COOKIES=/path/to/cookies.txt
AV_INGEST_PROXY_YTDLP_COOKIES_FROM_BROWSER=chrome
AV_INGEST_PROXY_YOUTUBE_COOKIES=/path/to/cookies.txt
AV_INGEST_PROXY_YOUTUBE_COOKIE_HEADER='LOGIN_INFO=...; SAPISID=...'
AV_INGEST_PROXY_YOUTUBE_VISITOR_DATA=...
AV_INGEST_PROXY_YOUTUBE_PLAYER_PO_TOKEN=...
AV_INGEST_PROXY_YOUTUBE_GVS_PO_TOKEN=...
AV_INGEST_PROXY_RESOLVE_MODE=transcribe
```

The native Rust YouTube resolver reads Netscape-format cookies from
`AV_INGEST_PROXY_YOUTUBE_COOKIES`. If that is unset, it reuses
`AV_INGEST_PROXY_YTDLP_COOKIES`. Auth cookies are sent to YouTube requests and
used to generate SAPISID InnerTube authorization headers.
If exporting a cookie file is inconvenient, pass a raw YouTube `Cookie` header
through `AV_INGEST_PROXY_YOUTUBE_COOKIE_HEADER`.

When YouTube requires GVS PO tokens, configure `yt-dlp` the same way you would on
the command line, for example through `AV_INGEST_PROXY_YTDLP_EXTRACTOR_ARGS` and
an installed `yt-dlp` PO-token provider plugin.
For the native resolver, pass known PO tokens through
`AV_INGEST_PROXY_YOUTUBE_PLAYER_PO_TOKEN` and
`AV_INGEST_PROXY_YOUTUBE_GVS_PO_TOKEN`; the GVS token is appended to resolved
Google Video format URLs.
The native resolver also reads `visitor_data` and `po_token` entries from
`AV_INGEST_PROXY_YTDLP_EXTRACTOR_ARGS` when present.

Use `AV_INGEST_PROXY_RESOLVE_MODE=transcribe` for ASR-only jobs. `/resolve`
then returns audio-only formats when YouTube provides them. If no audio-only
format is available, it keeps only the smallest muxed audio/video format.
The native resolver removes direct formats that still require YouTube JS
signature or `n` challenge solving, but it keeps streamable direct audio/video
URLs plus HLS/DASH manifest URLs. `/resolve` also includes a `streams` summary
with best muxed, video-only, audio-only, HLS, and DASH candidates when present.

Rust consumers can use `TranscribeAudioResolver::download_youtube_audio` for
cache-first ASR jobs. It asks `yt-dlp` to download one original compressed audio
format without post-processing or FFmpeg, and returns duration, format, MIME,
and file-size metadata for local decoders such as SoundKit.

## Notes

`crates/extractor` is a small browser-side helper used by the standalone test UI.
It is not involved in proxying, media decode, or `/frame` extraction.

## Checks

```bash
npm run check
```
