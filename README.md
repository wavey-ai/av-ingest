# av-ingest

Local Rust proxy for resolving public media URLs, proxying media bytes, and
extracting source frames by timestamp.

The main consumer is a browser app that needs safe preview playback plus an exact
source frame for rendering. Browser playback can use a lower-resolution compatible
stream; `/frame` can use the highest-resolution source stream.

## Requirements

```bash
brew install libvpx pkg-config
```

You also need Rust/Cargo. `ffmpeg` is optional and is only used as a fallback for
`/frame`.

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
timestamp. It can select a high-resolution WebM/VP9 stream, seek by WebM cues,
decode with `libvpx`, and return PNG.

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

1. Resolve source streams.
2. Pick the best video stream, preferring high-resolution WebM/VP9 when present.
3. Fetch enough bytes to parse WebM metadata and cues.
4. Range-fetch the cluster around `ts_us`.
5. Decode VP9 with `libvpx`.
6. Return PNG.

Fallbacks:

- If cue seeking fails, fetch the full WebM and decode natively.
- If native decode fails, use `ffmpeg` if available.

Environment flags:

```text
AV_INGEST_NATIVE_FRAME=0
AV_INGEST_FFMPEG=/path/to/ffmpeg
AV_INGEST_FRAME_TIMEOUT_SECONDS=75
```

## Notes

`crates/extractor` is a small browser-side helper used by the standalone test UI.
It is not involved in proxying, media decode, or `/frame` extraction.

## Checks

```bash
npm run check
```
