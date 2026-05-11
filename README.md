# av-ingest

Repo: <https://github.com/wavey-ai/av-ingest>

Demo: <https://wavey.ai/code/av-ingest/>

Media proxy health: <https://av-proxy.wavey.ai/healthz>

Browser-first audio/video ingest for the scratch.fm frame-selection flow. Paste a
public URL, load a browser-playable video, play or seek in the browser, pause on
an exact frame, and copy that frame to a canvas.

The browser owns playback and frame capture. The server side only resolves media
metadata and streams byte ranges with CORS headers. There is no media storage and
no server-side transcoding in this path.

## Architecture

- `public/` is the browser UI served at `/code/av-ingest/`.
- `worker/worker.js` runs on Cloudflare Workers for UI asset serving, direct
  media proxying, and YouTube metadata resolution at `/api/av-ingest/...`.
- `crates/extractor` is the small WASM helper used by the browser to classify
  source URLs and choose browser-playable formats from a YouTube player response.
- `crates/proxy` is the Rust media proxy for production YouTube media bytes.
  It uses `web-service` directly and streams `reqwest` chunks to the response
  writer with range support.

Cloudflare Worker egress to `googlevideo.com` currently returns 403 for the media
byte path, so the hosted UI defaults to `https://av-proxy.wavey.ai` for media
fetches. Local Wrangler runs still use the same-origin Worker proxy unless you
pass `?mediaProxy=https://...`.

## Supported Inputs

- Direct browser-playable `.mp4`, `.m4v`, `.mov`, `.webm`, `.m3u8`, and `.mpd`
  URLs through the proxy.
- Public YouTube URLs when YouTube exposes a browser-playable progressive video
  format.
- YouTube `n` and signature challenge solving in a sandboxed browser Worker.
  Cloudflare fetches metadata and player JavaScript; it does not execute the
  player script.

SoundCloud resolution is still stubbed. Private, Premium, cookie-gated, DRM, and
server-transcoded flows are outside this repo.

## Local UI

```bash
npm install
npm run build:wasm
npm run dev
```

Open:

```text
http://127.0.0.1:8789/
```

To test the UI against a separate media proxy:

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

Smoke test:

```bash
curl -kfsS https://127.0.0.1:8444/healthz

encoded_url="$(node -e 'console.log(encodeURIComponent(process.argv[1]))' \
  'https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4')"
curl -kfsS -H 'Range: bytes=0-1023' \
  "https://127.0.0.1:8444/proxy?url=${encoded_url}" \
  -o /tmp/flower.bin
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

## Current Measurements

Measured locally on May 11, 2026 with a live YouTube `itag=18` media URL resolved
by the deployed Worker. Payload size was `7,232,158` bytes.

| Path | Runs | Status | Avg Time | Median Time | Notes |
| --- | ---: | --- | ---: | ---: | --- |
| Rust proxy, full object | 5 | 200 | 3.13 s | 2.92 s | Browser path uses this in production |
| Direct full object | 5 | 200 | 3.46 s | 3.75 s | Baseline from the same local network |
| Rust proxy, 1 KB range | 1 | 206 | n/a | n/a | Returned `Content-Range` and exactly 1024 bytes |
| Cloudflare Worker googlevideo probe | repeated | 403 | n/a | n/a | Reason for the Rust proxy |

The proxy binary built on macOS was `9.9 MB`. The production container is a
single Rust process on Debian slim with no Python runtime.

## Deployment

UI and Worker:

```bash
CLOUDFLARE_EMAIL=jamie@wavey.ai \
CLOUDFLARE_API_KEY="$(tr -d '\n\r' < /Users/jamie/wavey.ai/.cloudflare-token)" \
npx wrangler deploy
```

Rust media proxy, intended permanent path:

```bash
gh workflow run deploy-av-ingest-proxy.yml \
  -R wavey-ai/bitneedle \
  -f av_ingest_ref=main
```

The proxy workflow lives in `wavey-ai/bitneedle` because the kubeadm and
Cloudflare deployment secrets are already attached to that repo. It builds
`ghcr.io/wavey-ai/av-ingest-proxy`, deploys the Kubernetes manifests under
`deploy/k8s/av-ingest-proxy`, creates `av-proxy.wavey.ai`, and verifies health
plus a byte-range media fetch.

Current live proxy:

```bash
cloudflared tunnel --config .cloudflared/config.yml run
```

This points `av-proxy.wavey.ai` at the local Rust proxy through Cloudflare
Tunnel. It is useful while the bitneedle GitHub Actions runner path is returning
`startup_failure` before jobs are scheduled.

## upload-response

`upload-response` is not used for the media proxy. It is the right abstraction
when an ingress stream needs to be handed to local or remote workers and a later
worker response must be bridged back to the client.

For this service there is no worker stage and no request body to cache. The fast
path is direct `web-service` routing plus upstream `reqwest` streaming, which
avoids cache-slot allocation, polling, and extra body copies.
