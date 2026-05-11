import { JsAnalyzer, JsExtractor, JsMatchers } from "youtubei.js";

const ALLOWED_HOSTS = [
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
];

const DEFAULT_FETCH_HEADERS = {
  "Accept-Language": "en-US,en;q=0.9",
  "User-Agent":
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36",
};
const ANDROID_FETCH_HEADERS = {
  ...DEFAULT_FETCH_HEADERS,
  "User-Agent": "com.google.android.youtube/21.03.36(Linux; U; Android 16; en_US; SM-S908E Build/TP1A.220624.014) gzip",
};
const IOS_FETCH_HEADERS = {
  ...DEFAULT_FETCH_HEADERS,
  "User-Agent": "com.google.ios.youtube/20.11.6 (iPhone10,4; U; CPU iOS 16_7_7 like Mac OS X)",
};
const TV_FETCH_HEADERS = {
  ...DEFAULT_FETCH_HEADERS,
  "User-Agent": "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version",
};

const UI_PREFIX = "/code/av-ingest";
const API_PREFIX = "/api/av-ingest";
const RESOLVE_CACHE_TTL_SECONDS = 180;
const YOUTUBE_PLAYER_VARIANT = "player_es6.vflset/en_US/base.js";
const YOUTUBE_WEB_API_KEY = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";
const YOUTUBE_INNERTUBE_CLIENTS = [
  {
    id: "android",
    headers: ANDROID_FETCH_HEADERS,
    client: {
      clientName: "ANDROID",
      clientVersion: "21.03.36",
      androidSdkVersion: 36,
      osName: "Android",
      osVersion: "13",
      platform: "MOBILE",
      clientFormFactor: "SMALL_FORM_FACTOR",
      userAgent: ANDROID_FETCH_HEADERS["User-Agent"],
      hl: "en",
      gl: "US",
    },
  },
  {
    id: "android_embedded",
    headers: ANDROID_FETCH_HEADERS,
    client: {
      clientName: "ANDROID",
      clientVersion: "21.03.36",
      androidSdkVersion: 36,
      osName: "Android",
      osVersion: "13",
      platform: "MOBILE",
      clientFormFactor: "SMALL_FORM_FACTOR",
      clientScreen: "EMBED",
      userAgent: ANDROID_FETCH_HEADERS["User-Agent"],
      hl: "en",
      gl: "US",
    },
    thirdParty: { embedUrl: "https://www.youtube.com" },
  },
  {
    id: "ios",
    headers: IOS_FETCH_HEADERS,
    client: {
      clientName: "iOS",
      clientVersion: "20.11.6",
      deviceMake: "Apple",
      deviceModel: "iPhone10,4",
      osName: "iOS",
      osVersion: "16.7.7.20H330",
      platform: "MOBILE",
      userAgent: IOS_FETCH_HEADERS["User-Agent"],
      hl: "en",
      gl: "US",
    },
  },
  {
    id: "ios_embedded",
    headers: IOS_FETCH_HEADERS,
    client: {
      clientName: "iOS",
      clientVersion: "20.11.6",
      deviceMake: "Apple",
      deviceModel: "iPhone10,4",
      osName: "iOS",
      osVersion: "16.7.7.20H330",
      platform: "MOBILE",
      clientScreen: "EMBED",
      userAgent: IOS_FETCH_HEADERS["User-Agent"],
      hl: "en",
      gl: "US",
    },
    thirdParty: { embedUrl: "https://www.youtube.com" },
  },
  {
    id: "android_vr",
    headers: {
      ...DEFAULT_FETCH_HEADERS,
      "User-Agent":
        "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
    },
    client: {
      clientName: "ANDROID_VR",
      clientVersion: "1.65.10",
      androidSdkVersion: 32,
      deviceMake: "Oculus",
      deviceModel: "Quest 3",
      osName: "Android",
      osVersion: "12L",
      platform: "MOBILE",
      clientFormFactor: "SMALL_FORM_FACTOR",
      hl: "en",
      gl: "US",
    },
  },
  {
    id: "tv_embedded",
    headers: TV_FETCH_HEADERS,
    client: {
      clientName: "TVHTML5_SIMPLY_EMBEDDED_PLAYER",
      clientVersion: "2.0",
      clientScreen: "EMBED",
      hl: "en",
      gl: "US",
    },
    thirdParty: { embedUrl: "https://www.youtube.com" },
  },
  {
    id: "web_embedded",
    headers: DEFAULT_FETCH_HEADERS,
    client: {
      clientName: "WEB_EMBEDDED_PLAYER",
      clientVersion: "1.20260206.01.00",
      clientScreen: "EMBED",
      hl: "en",
      gl: "US",
    },
    thirdParty: { embedUrl: "https://www.google.com/" },
  },
];

export default {
  async fetch(request, env) {
    try {
      if (request.method === "OPTIONS") {
        return optionsResponse();
      }

      const originalUrl = new URL(request.url);
      if (originalUrl.pathname === UI_PREFIX) {
        return Response.redirect(`${originalUrl.origin}${UI_PREFIX}/${originalUrl.search}`, 308);
      }

      const apiUrl = normalizeApiUrl(originalUrl);
      if (apiUrl) {
        if (apiUrl.pathname === "/resolve") {
          return resolveSource(apiUrl);
        }
        if (apiUrl.pathname === "/proxy") {
          return proxyRequest(request, apiUrl);
        }
        if (apiUrl.pathname === "/youtube-proxy") {
          return youtubeProxyRequest(request, apiUrl);
        }
        return textResponse("Not found", 404);
      }
      return serveAsset(request, env, normalizeAssetUrl(originalUrl));
    } catch (error) {
      if (error instanceof Response) {
        return error;
      }
      return textResponse(error?.message || "Internal error", 500);
    }
  },
};

async function resolveSource(requestUrl) {
  const sourceUrl = requireAllowedUrl(requestUrl.searchParams.get("url") || "");
  if (!isYoutubeHost(sourceUrl.hostname)) {
    return jsonResponse({ error: "Only YouTube resolve is implemented in this prototype." }, 400);
  }

  const videoId = extractYoutubeId(sourceUrl);
  if (!videoId) {
    return textResponse("Could not extract YouTube video id.", 400);
  }

  const bypassCache = requestUrl.searchParams.has("_") || requestUrl.searchParams.get("refresh") === "1";
  if (!bypassCache) {
    const cached = await readResolveCache(videoId);
    if (cached) {
      return jsonResponse({ ...cached, cached: true });
    }
  }

  let resolver = "watch";
  let watchStatus = null;
  let watchHtml = "";
  let playerResponse = null;
  const watchResponse = await upstreamFetch(sourceUrl, { headers: DEFAULT_FETCH_HEADERS });
  watchStatus = watchResponse.status;
  if (watchResponse.ok) {
    watchHtml = await watchResponse.text();
    playerResponse = extractInitialPlayerResponse(watchHtml);
  }
  if (!playerResponse?.streamingData || playerResponseNeedsChallenge(playerResponse)) {
    const watchPlayerResponse = playerResponse;
    const resolved = await fetchBestInnertubePlayerResponse(videoId);
    resolver = resolved.resolver;
    playerResponse = resolved.playerResponse;
    if (watchPlayerResponse?.streamingData && playerResponseNeedsChallenge(playerResponse)) {
      resolver = "watch";
      playerResponse = watchPlayerResponse;
    }
  }
  const playerChallenge = playerResponseNeedsChallenge(playerResponse)
    ? await extractPlayerChallenge(watchHtml)
    : null;

  const payload = {
    provider: "youtube",
    resolver,
    watchStatus,
    url: sourceUrl.toString(),
    title: playerResponse?.videoDetails?.title || null,
    durationSeconds: Number(playerResponse?.videoDetails?.lengthSeconds || 0) || null,
    playerChallenge,
    playerResponse,
  };
  await writeResolveCache(videoId, payload);
  return jsonResponse(payload);
}

async function youtubeProxyRequest(request, requestUrl) {
  if (request.method !== "GET" && request.method !== "HEAD") {
    return textResponse("Method not allowed", 405);
  }

  const videoId = cleanYoutubeId(requestUrl.searchParams.get("videoId") || "");
  const itag = Number(requestUrl.searchParams.get("itag") || 0);
  if (!videoId || !Number.isSafeInteger(itag) || itag <= 0) {
    return textResponse("Invalid YouTube media request", 400);
  }

  const preferredClient = requestUrl.searchParams.get("client") || "";
  const resolved = await fetchBestInnertubePlayerResponse(videoId, preferredClient);
  const format = findFormatForMediaRequest(resolved.playerResponse, itag);
  if (!format?.url) {
    return textResponse(`No YouTube media format was available from ${resolved.resolver}`, 502);
  }

  const upstreamUrl = new URL(format.url);
  const headers = new Headers(mediaFetchHeaders(upstreamUrl));
  const range = request.headers.get("Range");
  if (range) {
    headers.set("Range", range);
  }

  const upstream = await upstreamFetch(upstreamUrl, {
    method: request.method,
    headers,
    redirect: "follow",
  });
  const response = withCors(upstream);
  response.headers.set("X-AV-Ingest-Resolver", resolved.resolver);
  response.headers.set("X-AV-Ingest-Itag", String(format.itag || ""));
  return response;
}

async function fetchBestInnertubePlayerResponse(videoId, preferredClient = "") {
  let fallback = null;
  const attempts = [];
  for (const clientConfig of orderedInnertubeClients(preferredClient)) {
    try {
      const playerResponse = await fetchInnertubePlayerResponse(videoId, clientConfig);
      const status = playerResponse?.playabilityStatus?.status || null;
      const hasStreamingData = Boolean(playerResponse?.streamingData);
      const hasVideoStreams = playerResponseHasStreams(playerResponse);
      attempts.push({ client: clientConfig.id, status, hasStreamingData, hasVideoStreams });
      fallback = { resolver: `innertube_${clientConfig.id}`, playerResponse, attempts };
      if (hasVideoStreams && !playerResponseNeedsChallenge(playerResponse)) {
        playerResponse.__avIngestAttempts = attempts;
        return fallback;
      }
    } catch (error) {
      attempts.push({ client: clientConfig.id, error: error?.message || String(error) });
    }
  }
  if (fallback) {
    fallback.playerResponse.__avIngestAttempts = attempts;
    return fallback;
  }
  throw new Error(`YouTube InnerTube player fetch failed: ${attempts.map((attempt) => `${attempt.client}: ${attempt.error || attempt.status}`).join(", ")}`);
}

function orderedInnertubeClients(preferredClient) {
  const preferred = preferredClient.replace(/^innertube_/, "");
  const first = YOUTUBE_INNERTUBE_CLIENTS.find((client) => client.id === preferred);
  if (!first) {
    return YOUTUBE_INNERTUBE_CLIENTS;
  }
  return [
    first,
    ...YOUTUBE_INNERTUBE_CLIENTS.filter((client) => client.id !== preferred),
  ];
}

function findFormatForMediaRequest(playerResponse, itag) {
  const streamingData = playerResponse?.streamingData;
  const formats = [
    ...(streamingData?.formats || []),
    ...(streamingData?.adaptiveFormats || []),
  ].filter((format) => format?.url && format?.mimeType?.startsWith("video/"));
  return formats.find((format) => Number(format.itag) === itag) || formats.sort(formatScoreForProxy)[0];
}

function formatScoreForProxy(a, b) {
  return proxyFormatScore(b) - proxyFormatScore(a);
}

function proxyFormatScore(format) {
  const height = Number(format.height || 0);
  const fps = Number(format.fps || 30);
  const mime = format.mimeType || "";
  const audioBonus = format.audioQuality || mime.includes("mp4a") ? 100_000 : 0;
  const codecBonus = mime.includes("avc1") ? 500 : mime.includes("vp9") || mime.includes("av01") ? 300 : 0;
  return audioBonus + height * 10 + Math.min(fps, 60) + codecBonus;
}

async function fetchInnertubePlayerResponse(videoId, clientConfig) {
  const response = await upstreamFetch(
    new URL(`https://youtubei.googleapis.com/youtubei/v1/player?key=${YOUTUBE_WEB_API_KEY}`),
    {
      method: "POST",
      headers: {
        ...clientConfig.headers,
        "Content-Type": "application/json",
        Origin: "https://www.youtube.com",
        Referer: "https://www.youtube.com/",
      },
      body: JSON.stringify({
        videoId,
        contentCheckOk: true,
        racyCheckOk: true,
        context: {
          client: clientConfig.client,
          ...(clientConfig.thirdParty ? { thirdParty: clientConfig.thirdParty } : {}),
        },
        playbackContext: {
          contentPlaybackContext: {
            html5Preference: "HTML5_PREF_WANTS",
          },
        },
      }),
    },
  );
  if (!response.ok) {
    throw new Error(`YouTube InnerTube player fetch failed: ${response.status}`);
  }
  return response.json();
}

async function proxyRequest(request, requestUrl) {
  if (request.method !== "GET" && request.method !== "HEAD") {
    return textResponse("Method not allowed", 405);
  }
  const upstreamUrl = requireAllowedUrl(requestUrl.searchParams.get("url") || "");
  const headers = new Headers(mediaFetchHeaders(upstreamUrl));
  const range = request.headers.get("Range");
  if (range) {
    headers.set("Range", range);
  }
  const upstream = await upstreamFetch(upstreamUrl, {
    method: request.method,
    headers,
    redirect: "follow",
  });
  return withCors(upstream);
}

function mediaFetchHeaders(url) {
  const client = url.searchParams.get("c");
  if (client === "ANDROID" || client === "ANDROID_VR") {
    return ANDROID_FETCH_HEADERS;
  }
  if (client === "IOS") {
    return IOS_FETCH_HEADERS;
  }
  if (client?.startsWith("TVHTML5")) {
    return TV_FETCH_HEADERS;
  }
  return DEFAULT_FETCH_HEADERS;
}

async function readResolveCache(videoId) {
  try {
    const response = await caches.default.match(resolveCacheKey(videoId));
    return response ? response.json() : null;
  } catch {
    return null;
  }
}

async function writeResolveCache(videoId, payload) {
  if (!playerResponseHasStreams(payload.playerResponse)) {
    return;
  }
  try {
    await caches.default.put(
      resolveCacheKey(videoId),
      new Response(JSON.stringify(payload), {
        headers: {
          "Content-Type": "application/json; charset=utf-8",
          "Cache-Control": `public, max-age=${RESOLVE_CACHE_TTL_SECONDS}`,
        },
      }),
    );
  } catch {
    // The local dev runtime may not provide the same Cache API behavior as Workers.
  }
}

function resolveCacheKey(videoId) {
  return new Request(`https://av-ingest.local/resolve/youtube/${videoId}`);
}

function playerResponseHasStreams(playerResponse) {
  const streamingData = playerResponse?.streamingData;
  const formats = [
    ...(streamingData?.formats || []),
    ...(streamingData?.adaptiveFormats || []),
  ];
  return formats.some((format) => String(format?.mimeType || "").startsWith("video/"));
}

function normalizeApiUrl(url) {
  const normalized = new URL(url);
  if (normalized.pathname === API_PREFIX) {
    normalized.pathname = "/";
    return normalized;
  }
  if (normalized.pathname.startsWith(`${API_PREFIX}/`)) {
    normalized.pathname = normalized.pathname.slice(API_PREFIX.length) || "/";
    return normalized;
  }

  // Local/back-compat routes while iterating with `wrangler dev`.
  for (const path of ["/api/resolve", "/api/proxy", "/api/youtube-proxy"]) {
    if (normalized.pathname === path) {
      normalized.pathname = normalized.pathname.slice("/api".length) || "/";
      return normalized;
    }
  }
  return null;
}

function normalizeAssetUrl(url) {
  const normalized = new URL(url);
  if (normalized.pathname.startsWith(`${UI_PREFIX}/`)) {
    normalized.pathname = normalized.pathname.slice(UI_PREFIX.length) || "/";
  }
  return normalized;
}

function serveAsset(request, env, url) {
  return env.ASSETS.fetch(new Request(url, request));
}

function extractInitialPlayerResponse(html) {
  const marker = "ytInitialPlayerResponse";
  const markerIndex = html.indexOf(marker);
  if (markerIndex < 0) {
    return null;
  }
  const equalsIndex = html.indexOf("=", markerIndex);
  if (equalsIndex < 0) {
    return null;
  }
  const start = html.indexOf("{", equalsIndex);
  if (start < 0) {
    return null;
  }

  let depth = 0;
  let inString = false;
  let escape = false;
  for (let index = start; index < html.length; index += 1) {
    const ch = html[index];
    if (inString) {
      if (escape) {
        escape = false;
      } else if (ch === "\\") {
        escape = true;
      } else if (ch === "\"") {
        inString = false;
      }
      continue;
    }
    if (ch === "\"") {
      inString = true;
      continue;
    }
    if (ch === "{") {
      depth += 1;
      continue;
    }
    if (ch === "}") {
      depth -= 1;
      if (depth === 0) {
        return JSON.parse(html.slice(start, index + 1));
      }
    }
  }
  return null;
}

async function extractPlayerChallenge(watchHtml) {
  const playerId = extractPlayerId(watchHtml) || await fetchCurrentPlayerId();
  if (!playerId) {
    return null;
  }
  const playerUrl = `https://www.youtube.com/s/player/${playerId}/${YOUTUBE_PLAYER_VARIANT}`;
  const playerResponse = await upstreamFetch(new URL(playerUrl), { headers: DEFAULT_FETCH_HEADERS });
  if (!playerResponse.ok) {
    return {
      playerId,
      playerUrl,
      error: `Player JS fetch failed: ${playerResponse.status}`,
    };
  }

  const playerJs = await playerResponse.text();
  const analyzer = new JsAnalyzer(playerJs, {
    extractions: [{ friendlyName: "nsigFunction", match: JsMatchers.nsigMatcher }],
  });
  const extractor = new JsExtractor(analyzer);
  const script = extractor.buildScript({ disallowSideEffectInitializers: true });
  return {
    playerId,
    playerUrl,
    exported: script.exported,
    script: script.output,
  };
}

function extractPlayerId(html) {
  const normalized = html.replaceAll("\\/", "/");
  const match = normalized.match(/\/s\/player\/([a-fA-F0-9]{8,})\//);
  return match?.[1] || null;
}

async function fetchCurrentPlayerId() {
  const response = await upstreamFetch(new URL("https://www.youtube.com/iframe_api"), {
    headers: DEFAULT_FETCH_HEADERS,
  });
  if (!response.ok) {
    return null;
  }
  return extractPlayerId(await response.text());
}

function extractYoutubeId(url) {
  const host = url.hostname.toLowerCase().replace(/^www\./, "");
  if (host === "youtu.be") {
    return cleanYoutubeId(url.pathname.split("/").filter(Boolean)[0]);
  }
  const queryId = url.searchParams.get("v");
  if (queryId) {
    return cleanYoutubeId(queryId);
  }
  const segments = url.pathname.split("/").filter(Boolean);
  for (const prefix of ["shorts", "embed", "live", "v"]) {
    const index = segments.indexOf(prefix);
    if (index >= 0) {
      return cleanYoutubeId(segments[index + 1]);
    }
  }
  return null;
}

function cleanYoutubeId(value) {
  const id = String(value || "").match(/^[A-Za-z0-9_-]{11}/)?.[0] || "";
  return id.length === 11 ? id : null;
}

function playerResponseNeedsChallenge(playerResponse) {
  const streamingData = playerResponse?.streamingData;
  if (!streamingData) {
    return false;
  }
  return [
    ...(streamingData.formats || []),
    ...(streamingData.adaptiveFormats || []),
  ].some(formatNeedsChallenge);
}

function formatNeedsChallenge(format) {
  if (format.signatureCipher || format.cipher) {
    return true;
  }
  if (!format.url) {
    return false;
  }
  try {
    return new URL(format.url).searchParams.has("n");
  } catch {
    return false;
  }
}

function requireAllowedUrl(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Response("Invalid URL", { status: 400 });
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") {
    throw new Response("Unsupported URL protocol", { status: 400 });
  }
  if (isBlockedHost(url.hostname)) {
    throw new Response(`Host not allowed: ${url.hostname}`, { status: 403 });
  }
  if (!isAllowedHost(url.hostname) && !isDirectMediaPath(url.pathname)) {
    throw new Response(`Host not allowed: ${url.hostname}`, { status: 403 });
  }
  return url;
}

function isBlockedHost(hostname) {
  const host = hostname.toLowerCase();
  return (
    host === "localhost" ||
    host === "0.0.0.0" ||
    host === "127.0.0.1" ||
    host === "::1" ||
    host.startsWith("127.") ||
    host.startsWith("10.") ||
    host.startsWith("192.168.") ||
    /^172\.(1[6-9]|2\d|3[0-1])\./.test(host)
  );
}

function isAllowedHost(hostname) {
  const host = hostname.toLowerCase();
  return ALLOWED_HOSTS.some((allowed) => host === allowed || host.endsWith(`.${allowed}`));
}

function isDirectMediaPath(pathname) {
  return /\.(mp4|m4v|mov|webm|m3u8|mpd)$/i.test(pathname);
}

function isYoutubeHost(hostname) {
  const host = hostname.toLowerCase();
  return (
    host === "youtube.com" ||
    host.endsWith(".youtube.com") ||
    host === "youtube-nocookie.com" ||
    host.endsWith(".youtube-nocookie.com") ||
    host === "youtu.be"
  );
}

function upstreamFetch(url, init) {
  return fetch(url, {
    ...init,
    cf: {
      cacheTtl: 0,
      cacheEverything: false,
    },
  });
}

function withCors(response) {
  const headers = new Headers(response.headers);
  headers.set("Access-Control-Allow-Origin", "*");
  headers.set("Access-Control-Allow-Headers", "Range, Content-Type");
  headers.set("Access-Control-Expose-Headers", "Accept-Ranges, Content-Length, Content-Range, Content-Type");
  headers.set("Cache-Control", "no-store");
  headers.delete("Cross-Origin-Resource-Policy");
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}

function jsonResponse(value, status = 200) {
  return withCors(
    new Response(`${JSON.stringify(value, null, 2)}\n`, {
      status,
      headers: { "Content-Type": "application/json; charset=utf-8" },
    }),
  );
}

function textResponse(value, status = 200) {
  return withCors(
    new Response(`${value}\n`, {
      status,
      headers: { "Content-Type": "text/plain; charset=utf-8" },
    }),
  );
}

function optionsResponse() {
  return withCors(new Response(null, { status: 204 }));
}
