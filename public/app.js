import init, {
  parseSourceUrl,
  selectBrowserVideoFormats,
} from "./pkg/av_ingest_extractor.js?v=20260511a";

const els = {
  sourceUrl: document.querySelector("#source-url"),
  loadSource: document.querySelector("#load-source"),
  status: document.querySelector("#status"),
  video: document.querySelector("#video"),
  prevFrame: document.querySelector("#prev-frame"),
  selectFrame: document.querySelector("#select-frame"),
  nextFrame: document.querySelector("#next-frame"),
  formatSelect: document.querySelector("#format-select"),
  timeReadout: document.querySelector("#time-readout"),
  videoReadout: document.querySelector("#video-readout"),
  proxyReadout: document.querySelector("#proxy-readout"),
  previewCanvas: document.querySelector("#preview-canvas"),
  selectedCanvas: document.querySelector("#selected-canvas"),
  log: document.querySelector("#log"),
};

const API_BASE_PATH = "/api/av-ingest";
const ASSET_VERSION = "20260511a";
const PUBLIC_MEDIA_PROXY_BASE_URL = "https://av-proxy.wavey.ai";
const MEDIA_PROXY_BASE_URL = resolveMediaProxyBaseUrl();
let wasmReady;
let currentFormats = [];
let youtubeChallengeWorker;
let youtubeChallengeMessageId = 0;
const youtubeChallengePending = new Map();

if (els.proxyReadout) {
  els.proxyReadout.textContent = `media proxy ${MEDIA_PROXY_BASE_URL}`;
}

els.loadSource.addEventListener("click", () => {
  loadSource().catch((error) => {
    setStatus(error?.message ?? String(error), "error");
    log(error?.stack ?? String(error));
  });
});

els.sourceUrl.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    els.loadSource.click();
  }
});

els.formatSelect.addEventListener("change", () => {
  const format = currentFormats[Number(els.formatSelect.value)];
  if (format) {
    attachVideo(format).catch((error) => {
      setStatus(error?.message ?? String(error), "error");
      log(error?.stack ?? String(error));
    });
  }
});

els.video.addEventListener("loadedmetadata", syncVideoUi);
els.video.addEventListener("loadeddata", () => {
  syncVideoUi();
  drawVideoFrame(els.previewCanvas);
});
els.video.addEventListener("timeupdate", syncVideoUi);
els.video.addEventListener("seeked", () => {
  syncVideoUi();
  drawVideoFrame(els.previewCanvas);
});
els.video.addEventListener("pause", () => {
  syncVideoUi();
  selectFrame();
});
els.video.addEventListener("play", syncVideoUi);
els.video.addEventListener("error", () => {
  const mediaError = els.video.error;
  setStatus(mediaError?.message || `Video error ${mediaError?.code || ""}`.trim(), "error");
  syncVideoUi();
});

els.prevFrame.addEventListener("click", () => stepFrame(-1));
els.nextFrame.addEventListener("click", () => stepFrame(1));
els.selectFrame.addEventListener("click", () => selectFrame());

async function loadSource() {
  await initWasm();
  const source = parseSourceUrl(els.sourceUrl.value);
  log({ source });
  setStatus(`Resolving ${source.provider} source`);

  if (source.directMedia || source.provider === "direct") {
    currentFormats = [
      {
        label: "Direct media",
        url: proxyUrl(source.canonicalUrl),
        mimeType: null,
        width: null,
        height: null,
        fps: null,
      },
    ];
    renderFormats();
    await attachFirstAvailable();
    return;
  }

  if (source.provider === "youtube") {
    const resolved = await resolveYouTube(source.canonicalUrl);
    const formats = selectYouTubeFormats(resolved);
    currentFormats = await resolveYouTubeFormats(formats, resolved, source);
    renderFormats();
    await attachFirstAvailable();
    log({ source, selectedFormats: currentFormats, playerResponseSummary: summarizePlayerResponse(resolved) });
    return;
  }

  throw new Error(`${source.provider} resolution is not implemented in this browser prototype yet.`);
}

async function resolveYouTube(url) {
  const params = new URLSearchParams({
    url,
    _: String(Date.now()),
  });
  const response = await fetch(`${API_BASE_PATH}/resolve?${params}`, { cache: "no-store" });
  if (!response.ok) {
    throw new Error(await response.text());
  }
  return response.json();
}

async function attachVideo(format) {
  if (!format) {
    throw new Error("No playable format available.");
  }
  await probeFormat(format);
  els.video.pause();
  els.video.removeAttribute("src");
  els.video.load();
  els.video.src = format.url;
  els.video.type = format.mimeType || "";
  els.video.load();
  setStatus(`Loaded ${format.label}`);
  syncVideoUi();
}

async function attachFirstAvailable() {
  let lastError;
  for (let index = 0; index < currentFormats.length; index += 1) {
    els.formatSelect.value = String(index);
    try {
      await attachVideo(currentFormats[index]);
      return;
    } catch (error) {
      lastError = error;
      log(`Skipping ${currentFormats[index]?.label || `format ${index + 1}`}: ${error.message}`);
    }
  }
  throw lastError || new Error("No playable format available.");
}

async function probeFormat(format) {
  setStatus(`Probing ${format.label}`);
  const response = await fetch(format.url, { cache: "no-store", headers: { Range: "bytes=0-1023" } });
  if (response.ok || response.status === 206) {
    return;
  }
  throw new Error(`Media probe failed (${response.status}) for ${format.label}.`);
}

function renderFormats() {
  els.formatSelect.innerHTML = "";
  currentFormats.forEach((format, index) => {
    const option = document.createElement("option");
    option.value = String(index);
    option.textContent = format.label;
    els.formatSelect.append(option);
  });
  els.formatSelect.disabled = currentFormats.length <= 1;
}

async function resolveYouTubeFormats(formats, resolved, source) {
  const playerChallenge = resolved.playerChallenge;
  if (!Array.isArray(formats) || formats.length === 0) {
    throw new Error("No YouTube formats found.");
  }
  if (formats.some((format) => format.needsChallenge) && !playerChallenge?.script) {
    throw new Error(playerChallenge?.error || "YouTube player challenge script was not available.");
  }

  const solved = [];
  for (const format of formats) {
    try {
      if (!format.needsChallenge && format.itag && source.id) {
        if (format.url && MEDIA_PROXY_BASE_URL !== API_BASE_PATH) {
          solved.push({
            ...format,
            mediaUrl: format.url,
            url: proxyUrl(format.url),
          });
          continue;
        }
        solved.push({
          ...format,
          mediaUrl: null,
          url: youtubeProxyUrl(source.id, format.itag, resolved.resolver),
        });
        continue;
      }
      const mediaUrl = await resolveYouTubeMediaUrl(format, playerChallenge?.script || "");
      solved.push({
        ...format,
        mediaUrl,
        url: proxyUrl(mediaUrl),
        label: format.needsChallenge ? `${format.label} solved` : format.label,
      });
    } catch (error) {
      log(`Skipping ${format.label}: ${error.message}`);
    }
  }
  if (!solved.length) {
    throw new Error("No YouTube formats could be solved.");
  }
  return solved;
}

async function resolveYouTubeMediaUrl(format, challengeScript) {
  const cipherParams = format.signatureCipher || format.cipher
    ? new URLSearchParams(format.signatureCipher || format.cipher)
    : null;
  const rawUrl = cipherParams?.get("url") || format.url;
  if (!rawUrl) {
    throw new Error("Format does not include a media URL.");
  }

  const mediaUrl = new URL(rawUrl);
  const n = mediaUrl.searchParams.get("n") || "";
  const s = cipherParams?.get("s") || "";
  const sp = cipherParams?.get("sp") || "signature";
  if (n || s) {
    const result = await solveYoutubeChallenge({ script: challengeScript, n, s, sp });
    if (result.n) {
      mediaUrl.searchParams.set("n", result.n);
    }
    if (s) {
      if (!result.sig) {
        throw new Error("Signature challenge did not return a signature.");
      }
      mediaUrl.searchParams.set(sp, result.sig);
    }
  }
  return mediaUrl.toString();
}

function stepFrame(direction) {
  const video = els.video;
  if (!Number.isFinite(video.duration) || video.duration <= 0) {
    return;
  }
  if (!video.paused) {
    video.pause();
  }
  const fps = currentFormats[Number(els.formatSelect.value)]?.fps || 30;
  const step = 1 / fps;
  video.currentTime = Math.max(0, Math.min(video.duration, video.currentTime + direction * step));
  syncVideoUi();
}

function captureFrame({ quiet = false } = {}) {
  return selectFrame({ quiet });
}

function selectFrame({ quiet = false } = {}) {
  const ok = drawVideoFrame(els.selectedCanvas);
  if (ok) {
    drawVideoFrame(els.previewCanvas);
  }
  if (ok && !quiet) {
    setStatus(`Selected frame at ${formatTime(els.video.currentTime)}`);
  }
  syncVideoUi();
  return ok;
}

function drawVideoFrame(canvas) {
  const video = els.video;
  if (!video.videoWidth || !video.videoHeight || video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA) {
    return false;
  }
  if (canvas.width !== video.videoWidth || canvas.height !== video.videoHeight) {
    canvas.width = video.videoWidth;
    canvas.height = video.videoHeight;
  }
  const ctx = canvas.getContext("2d");
  try {
    ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
  } catch (error) {
    setStatus(`Frame capture failed: ${error.message}`, "error");
    return false;
  }
  return true;
}

function syncVideoUi() {
  const video = els.video;
  const hasVideo = Boolean(video.currentSrc);
  const hasFrameData = hasVideo && video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA;
  els.selectFrame.disabled = !hasFrameData;
  els.prevFrame.disabled = !hasFrameData;
  els.nextFrame.disabled = !hasFrameData;
  els.timeReadout.textContent = `${formatTime(video.currentTime || 0)} / ${formatTime(video.duration || 0)}`;
  els.videoReadout.textContent =
    video.videoWidth && video.videoHeight
      ? `${video.videoWidth} x ${video.videoHeight} | readyState ${video.readyState}`
      : hasVideo
        ? `loading | readyState ${video.readyState}`
        : "No video loaded";
}

function solveYoutubeChallenge({ script, n, s, sp }) {
  const worker = getYoutubeChallengeWorker();
  const id = ++youtubeChallengeMessageId;
  worker.postMessage({ id, script, n, s, sp });
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      youtubeChallengePending.delete(id);
      reject(new Error("YouTube challenge timed out."));
    }, 8000);
    youtubeChallengePending.set(id, { resolve, reject, timeout });
  });
}

function getYoutubeChallengeWorker() {
  if (youtubeChallengeWorker) {
    return youtubeChallengeWorker;
  }
  const workerCode = `
function getProcessor(n, sp, s) {
  return \`function process(n = "", sp = "", s = "") {
  const mockStreamingURL = "https://ytjs.googlevideo.com/videoplayback?expire=1234567890&" + "n=" + encodeURIComponent(n);
  const urlCtorFunction = exportedVars.nsigFunction || (() => { throw new Error("No n/sig decipher function extracted"); });
  const urlCtor = urlCtorFunction(mockStreamingURL, sp, s);
  const proto = Object.getPrototypeOf(urlCtor);
  const properties = Object.getOwnPropertyNames(proto);
  const methodBlacklist = ["constructor", "clone", "set", "get"];
  for (const prop of properties) {
    if (methodBlacklist.includes(prop)) continue;
    if (typeof urlCtor[prop] === "function") urlCtor[prop]();
  }
  const sigResult = urlCtor.get(sp);
  const nResult = urlCtor.get("n");
  return {
    sig: sigResult ? decodeURIComponent(sigResult) : undefined,
    n: nResult ? decodeURIComponent(nResult) : undefined
  };
}
return process(\${JSON.stringify(n || "")}, \${JSON.stringify(sp || "")}, \${JSON.stringify(s || "")});\`;
}
self.onmessage = (event) => {
  const { id, script, n, s, sp } = event.data;
  try {
    const result = new Function(script + "\\n" + getProcessor(n, sp, s))();
    self.postMessage({ id, result });
  } catch (error) {
    self.postMessage({ id, error: error?.message || String(error) });
  }
};
`;
  const url = URL.createObjectURL(new Blob([workerCode], { type: "text/javascript" }));
  youtubeChallengeWorker = new Worker(url);
  URL.revokeObjectURL(url);
  youtubeChallengeWorker.addEventListener("message", (event) => {
    const pending = youtubeChallengePending.get(event.data.id);
    if (!pending) {
      return;
    }
    clearTimeout(pending.timeout);
    youtubeChallengePending.delete(event.data.id);
    if (event.data.error) {
      pending.reject(new Error(event.data.error));
    } else {
      pending.resolve(event.data.result || {});
    }
  });
  youtubeChallengeWorker.addEventListener("error", (event) => {
    for (const [id, pending] of youtubeChallengePending) {
      clearTimeout(pending.timeout);
      pending.reject(new Error(event.message || "YouTube challenge worker failed."));
      youtubeChallengePending.delete(id);
    }
  });
  return youtubeChallengeWorker;
}

function proxyUrl(url) {
  return `${MEDIA_PROXY_BASE_URL}/proxy?url=${encodeURIComponent(url)}`;
}

function youtubeProxyUrl(videoId, itag, resolver) {
  const params = new URLSearchParams({
    videoId,
    itag: String(itag),
  });
  if (resolver) {
    params.set("client", resolver);
  }
  return `${API_BASE_PATH}/youtube-proxy?${params}`;
}

function selectYouTubeFormats(resolved) {
  try {
    return selectBrowserVideoFormats(JSON.stringify(resolved.playerResponse));
  } catch (error) {
    const summary = summarizePlayerResponse(resolved);
    log({
      error: error?.message || String(error),
      resolver: summary.resolver,
      cached: summary.cached,
      playability: summary.playabilityStatus,
      reason: summary.playabilityReason,
      title: summary.title,
      formats: summary.formats,
      adaptiveFormats: summary.adaptiveFormats,
      videoFormats: summary.videoFormats,
      attempts: summary.attempts,
    });
    throw new Error(
      `${error?.message || String(error)} (${summary.resolver || "unknown resolver"}, ` +
        `${summary.playabilityStatus || "unknown playability"}, ` +
        `${summary.videoFormats} video formats from ${summary.formats + summary.adaptiveFormats} total streams).`,
    );
  }
}

function summarizePlayerResponse(response) {
  const playerResponse = response?.playerResponse ?? response;
  const streamingData = playerResponse?.streamingData;
  const allFormats = [
    ...(streamingData?.formats || []),
    ...(streamingData?.adaptiveFormats || []),
  ];
  return {
    resolver: response?.resolver ?? null,
    cached: response?.cached ?? false,
    title: playerResponse?.videoDetails?.title ?? null,
    playabilityStatus: playerResponse?.playabilityStatus?.status ?? null,
    playabilityReason: playerResponse?.playabilityStatus?.reason ?? null,
    formats: streamingData?.formats?.length ?? 0,
    adaptiveFormats: streamingData?.adaptiveFormats?.length ?? 0,
    videoFormats: allFormats.filter((format) => String(format?.mimeType || "").startsWith("video/")).length,
    attempts: playerResponse?.__avIngestAttempts ?? [],
  };
}

function resolveMediaProxyBaseUrl() {
  const params = new URLSearchParams(window.location.search);
  const configured =
    params.get("mediaProxy") ||
    window.AV_INGEST_MEDIA_PROXY_BASE_URL ||
    "";
  if (configured) {
    return configured.replace(/\/+$/, "");
  }
  if (window.location.hostname === "wavey.ai" || window.location.hostname === "www.wavey.ai") {
    return PUBLIC_MEDIA_PROXY_BASE_URL;
  }
  return API_BASE_PATH;
}

function formatTime(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) {
    seconds = 0;
  }
  const minutes = Math.floor(seconds / 60);
  const whole = Math.floor(seconds % 60);
  const millis = Math.floor((seconds - Math.floor(seconds)) * 1000);
  return `${minutes}:${String(whole).padStart(2, "0")}.${String(millis).padStart(3, "0")}`;
}

async function initWasm() {
  if (!wasmReady) {
    wasmReady = init({ module_or_path: `./pkg/av_ingest_extractor_bg.wasm?v=${ASSET_VERSION}` });
  }
  return wasmReady;
}

function setStatus(message) {
  els.status.textContent = message;
}

function log(value) {
  els.log.textContent =
    typeof value === "string"
      ? value
      : JSON.stringify(value, null, 2);
}
