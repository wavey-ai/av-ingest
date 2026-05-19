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
2. Pick the best WebM/VP9 video stream.
3. Fetch enough bytes to parse WebM metadata and cues.
4. Range-fetch the cluster around `ts_us`.
5. Decode VP9 with `libvpx`.
6. Return PNG.

## Notes

`crates/extractor` is a small browser-side helper used by the standalone test UI.
It is not involved in proxying, media decode, or `/frame` extraction.

## Checks

```bash
npm run check
```
