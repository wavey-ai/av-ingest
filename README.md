# av-ingest

Repo: <https://github.com/wavey-ai/av-ingest>

Demo UI: <https://wavey.ai/code/av-ingest/>

Rust API/proxy health: <https://av-proxy.wavey.ai/healthz>

Browser-first audio/video ingest for frame-selection flows. Paste a public URL,
load a browser-playable video, play or seek in the browser, pause on an exact
frame, and either copy the browser-decoded preview frame to a canvas or ask the
Rust proxy for a high-resolution source frame at the same timestamp.

The production split is deliberate:

- Cloudflare serves the demo UI and provides DNS/TLS routing for public domains.
- Rust on Linode resolves media metadata and proxies media bytes.
- Cloudflare Workers are not used for YouTube/media proxying.

## Architecture

- `public/` is the browser UI served at `/code/av-ingest/`.
- `worker/worker.js` is only a tiny Cloudflare asset router for the demo UI.
  It does not expose `/resolve`, `/proxy`, `/youtube-proxy`, or any media API.
- `crates/extractor` is the WASM helper used by the browser to classify source
  URLs and choose browser-playable formats from a YouTube player response.
- `crates/proxy` is the Rust service deployed on Linode. It owns:
  - `GET /healthz`
  - `GET /resolve?url=...`
  - `GET|HEAD /proxy?url=...`
  - `GET /frame?url=...&ts_us=...`
  - HLS playlist rewriting so every playlist and segment request stays on the
    Rust proxy
  - byte-range forwarding with CORS headers for browser playback and canvas
    frame capture
  - native WebM/VP9 frame extraction for high-resolution stills

`av-proxy.wavey.ai` may still sit behind Cloudflare DNS/TLS, but the request
handler is the Rust service, not a Worker.

## Browser Preview Decode Path

The browser does the preview video decoding.

The UI attaches a proxied media URL to an HTML `<video>` element. Safari, Chrome,
and Firefox then use their native media pipeline to demux and decode the stream,
usually with hardware acceleration when the codec is supported by the device.
This repo does not decode video in JavaScript or WASM and does not run ffmpeg in
the browser.

Preview frame selection can use the decoded frame already held by the browser:

```js
ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
```

For MP4/WebM, the Rust proxy forwards byte ranges so the browser can seek and
buffer normally. For HLS, the Rust proxy rewrites master playlists, variant
playlists, and segment URLs to keep all media requests on `av-proxy.wavey.ai`.

## High-Resolution Frame Extraction

The browser preview path intentionally prefers broadly playable MP4/H.264 when
available. That keeps Safari and canvas capture reliable, but it is not always
the highest-resolution source that a provider exposes. In practice, high
resolution and 4K variants are often exposed as video-only WebM/VP9 streams,
while MP4/H.264 variants may stop at a lower resolution.

For record rendering, `GET /frame?url=...&ts_us=...` asks the Rust proxy to
extract a source frame at the requested microsecond timestamp instead of relying
on the browser preview frame. The endpoint resolves the source URL, chooses the
best high-resolution video stream, and returns an image suitable for drawing into
the record canvas.

The preferred native path is:

1. Resolve the media URL and select a high-resolution WebM/VP9 video stream.
2. Fetch a bounded initial byte range and parse EBML/WebM metadata.
3. Parse `Info`, `Tracks`, `SeekHead`, and `Cues`.
4. Convert cue cluster positions from Segment-relative offsets to absolute byte
   offsets.
5. Range-fetch only the cluster around the target timestamp.
6. Decode VP9 frames natively with `libvpx`.
7. Return a PNG frame.

If WebM cues are missing or too far from the initial byte range, the extractor
falls back to a full WebM fetch before native decode. If native decode fails, the
`/frame` endpoint can fall back to ffmpeg and return a JPEG. This fallback is for
operational resilience; the main high-resolution path is native Rust parsing
plus `libvpx` decode.

Useful environment flags:

- `AV_INGEST_NATIVE_FRAME=0` disables native WebM/VP9 extraction and uses the
  fallback path.
- `AV_INGEST_FFMPEG=/path/to/ffmpeg` overrides the fallback ffmpeg binary.
- `AV_INGEST_FRAME_TIMEOUT_SECONDS=75` controls fallback extraction timeout.

## Supported Inputs

- Direct browser-playable `.mp4`, `.m4v`, `.mov`, `.webm`, `.m3u8`, and `.mpd`
  URLs through the Rust proxy.
- Public YouTube URLs when YouTube exposes browser-playable progressive MP4
  formats, native HLS manifests, or high-resolution WebM/VP9 video streams for
  server-side frame extraction.

SoundCloud resolution is still stubbed. Private, Premium, cookie-gated, DRM, and
server-transcoded flows are outside this repo.

## Local UI

Build the WASM helper:

```bash
npm install
npm run build:wasm
```

Serve `public/` with any static server:

```bash
python3 -m http.server 8789 -d public
```

Open:

```text
http://127.0.0.1:8789/
```

By default the local UI uses `https://av-proxy.wavey.ai` for `/resolve` and
`/proxy`. To point it at a local proxy:

```text
http://127.0.0.1:8789/?mediaProxy=https://127.0.0.1:8444
```

## Local Rust Proxy

```bash
cargo build --release -p av-ingest-proxy

AV_INGEST_PROXY_PORT=8444 \
AV_INGEST_PROXY_TLS_CERT_PATH=/path/to/fullchain.pem \
AV_INGEST_PROXY_TLS_KEY_PATH=/path/to/privkey.pem \
target/release/av-ingest-proxy
```

For local HTTP development without TLS:

```bash
AV_INGEST_PROXY_LOCAL_HTTP=1 \
AV_INGEST_PROXY_PORT=8444 \
cargo run -p av-ingest-proxy
```

Smoke tests:

```bash
curl -kfsS https://127.0.0.1:8444/healthz

curl -kfsS \
  "https://127.0.0.1:8444/resolve?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3D6HMs7eoQkFw" \
  -o /tmp/resolve.json

encoded_url="$(node -e 'console.log(encodeURIComponent(process.argv[1]))' \
  'https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4')"
curl -kfsS -H 'Range: bytes=0-1023' \
  "https://127.0.0.1:8444/proxy?url=${encoded_url}" \
  -o /tmp/flower.bin

source_url="$(node -e 'console.log(encodeURIComponent(process.argv[1]))' \
  'https://www.youtube.com/watch?v=VIDEO_ID')"
curl -kfsS \
  "https://127.0.0.1:8444/frame?url=${source_url}&ts_us=114000000" \
  -o /tmp/frame.png
```

## Cloudflare UI Deploy

Cloudflare is only used for the demo UI route in this repo:

```bash
CLOUDFLARE_EMAIL=jamie@wavey.ai \
CLOUDFLARE_API_KEY="$(tr -d '\n\r' < /Users/jamie/wavey.ai/.cloudflare-token)" \
npx wrangler deploy
```

The Wrangler config routes only:

```text
wavey.ai/code/av-ingest*
www.wavey.ai/code/av-ingest*
```

There is no Cloudflare Worker media proxy route.

## Linode Deploy

The Rust proxy is the API/media service. The Linode installer builds the proxy,
installs a systemd service, and configures the public host:

```bash
deploy/linode/install-proxy.sh root@203.0.113.10 av-proxy.wavey.ai
```

The current live tunnel can also point `av-proxy.wavey.ai` at a local Rust proxy
while iterating:

```bash
cloudflared tunnel --config .cloudflared/config.yml run
```

## Checks

```bash
npm run check
```

This runs JavaScript syntax checks and `cargo test --workspace`.

Smoke inputs:

```text
https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4
https://www.youtube.com/watch?v=jNQXAC9IVRw
https://www.youtube.com/watch?v=dQw4w9WgXcQ
https://www.youtube.com/watch?v=6HMs7eoQkFw
```

## `web-service`

The Rust proxy uses the `web-service` crate and should continue to do so. This
service is a streaming/range proxy: requests arrive, the Rust service opens an
upstream `reqwest` stream, and chunks are written directly to the response via
`StreamWriter`.

`upload-response` is not used here. It is useful when a request body must be
handed to another local or remote worker and that worker's eventual response
must be bridged back. This proxy has no worker stage and no request body to
cache, so direct `web-service` routing avoids extra buffering, polling, and body
copies.

The proxy binary is a single Rust process. Native high-resolution frame
extraction links against `libvpx`. There is no Python runtime dependency in
production.
