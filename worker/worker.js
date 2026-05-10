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

export default {
  async fetch(request, env) {
    try {
      if (request.method === "OPTIONS") {
        return optionsResponse();
      }

      const url = new URL(request.url);
      if (url.pathname === "/api/resolve") {
        return resolveSource(request);
      }
      if (url.pathname === "/api/proxy") {
        return proxyRequest(request);
      }
      return env.ASSETS.fetch(request);
    } catch (error) {
      if (error instanceof Response) {
        return error;
      }
      return textResponse(error?.message || "Internal error", 500);
    }
  },
};

async function resolveSource(request) {
  const requestUrl = new URL(request.url);
  const sourceUrl = requireAllowedUrl(requestUrl.searchParams.get("url") || "");
  if (!isYoutubeHost(sourceUrl.hostname)) {
    return jsonResponse({ error: "Only YouTube resolve is implemented in this prototype." }, 400);
  }

  const watchResponse = await upstreamFetch(sourceUrl, { headers: DEFAULT_FETCH_HEADERS });
  const watchHtml = await watchResponse.text();
  if (!watchResponse.ok) {
    return textResponse(`YouTube watch fetch failed: ${watchResponse.status}`, watchResponse.status);
  }

  const playerResponse = extractInitialPlayerResponse(watchHtml);
  if (!playerResponse) {
    return textResponse("Could not find ytInitialPlayerResponse in YouTube page.", 422);
  }

  return jsonResponse({
    provider: "youtube",
    url: sourceUrl.toString(),
    title: playerResponse?.videoDetails?.title || null,
    durationSeconds: Number(playerResponse?.videoDetails?.lengthSeconds || 0) || null,
    playerResponse,
  });
}

async function proxyRequest(request) {
  if (request.method !== "GET" && request.method !== "HEAD") {
    return textResponse("Method not allowed", 405);
  }
  const requestUrl = new URL(request.url);
  const upstreamUrl = requireAllowedUrl(requestUrl.searchParams.get("url") || "");
  const headers = new Headers(DEFAULT_FETCH_HEADERS);
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
