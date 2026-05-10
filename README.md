# av-ingest

Browser-first audio/video ingest experiments for scratch.fm.

Local demo:

```text
http://127.0.0.1:8789/
```

The first prototype matches the scratch.fm video-background flow:

1. paste a URL
2. load a browser-playable video source
3. play or seek in the browser
4. pause on an exact frame
5. copy the current frame to a canvas

The browser owns playback and frame capture. A Cloudflare Worker is only a
CORS/range proxy plus a resolver. There is no media storage and no server-side
transcoding.

## Scope

- Direct browser-playable media URLs are working through the Worker range proxy.
- YouTube URL parsing and watch-page resolution are in place.
- YouTube playback is not complete yet: current YouTube streams require the
  player JS `n`/signature challenge solver before `googlevideo` accepts the
  proxied request.
- SoundCloud resolution is stubbed for a later pass.
- Private, Premium, cookie-based, DRM, and server-side transcoding flows are out
  of scope.

The next extractor milestone is the yt-dlp-like challenge layer: fetch the
YouTube player JS, solve `n` and signature challenges without relying on a paid
server process, then pass the solved media URL back through the Worker proxy for
range playback and canvas-safe frame capture.

## Local

```bash
npm install
npm run build:wasm
npm run dev
```

## Checks

```bash
npm run check
```

The current smoke path uses a direct MP4:

```text
https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4
```

## Relationship To bitneedle

`../bitneedle` already has the scratch.fm UI flow for local uploads and
server-fetched YouTube assets. This repo isolates the browser/serverless ingest
piece so it can be hardened independently before being wired back into
scratch.fm.
