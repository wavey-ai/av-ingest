import init, {
  parseSourceUrl,
  selectBrowserVideoFormats,
} from "./pkg/av_ingest_extractor.js";

const els = {
  sourceUrl: document.querySelector("#source-url"),
  loadSource: document.querySelector("#load-source"),
  status: document.querySelector("#status"),
  video: document.querySelector("#video"),
  prevFrame: document.querySelector("#prev-frame"),
  captureFrame: document.querySelector("#capture-frame"),
  nextFrame: document.querySelector("#next-frame"),
  formatSelect: document.querySelector("#format-select"),
  timeReadout: document.querySelector("#time-readout"),
  videoReadout: document.querySelector("#video-readout"),
  canvas: document.querySelector("#frame-canvas"),
  log: document.querySelector("#log"),
};

let wasmReady;
let currentFormats = [];

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
  captureFrame({ quiet: true });
});
els.video.addEventListener("timeupdate", syncVideoUi);
els.video.addEventListener("seeked", () => {
  syncVideoUi();
  captureFrame({ quiet: true });
});
els.video.addEventListener("pause", () => {
  syncVideoUi();
  captureFrame();
});
els.video.addEventListener("play", syncVideoUi);
els.video.addEventListener("error", () => {
  const mediaError = els.video.error;
  setStatus(mediaError?.message || `Video error ${mediaError?.code || ""}`.trim(), "error");
  syncVideoUi();
});

els.prevFrame.addEventListener("click", () => stepFrame(-1));
els.nextFrame.addEventListener("click", () => stepFrame(1));
els.captureFrame.addEventListener("click", () => captureFrame());

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
    await attachVideo(currentFormats[0]);
    return;
  }

  if (source.provider === "youtube") {
    const playerResponse = await resolveYouTube(source.canonicalUrl);
    const formats = selectBrowserVideoFormats(JSON.stringify(playerResponse.playerResponse));
    currentFormats = formats.map((format) => ({
      ...format,
      url: proxyUrl(format.url),
    }));
    renderFormats();
    await attachVideo(currentFormats[0]);
    log({ source, selectedFormats: currentFormats, playerResponseSummary: summarizePlayerResponse(playerResponse) });
    return;
  }

  throw new Error(`${source.provider} resolution is not implemented in this browser prototype yet.`);
}

async function resolveYouTube(url) {
  const response = await fetch(`/api/resolve?url=${encodeURIComponent(url)}`);
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

async function probeFormat(format) {
  setStatus(`Probing ${format.label}`);
  const response = await fetch(format.url, { headers: { Range: "bytes=0-1023" } });
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
  const video = els.video;
  if (!video.videoWidth || !video.videoHeight || video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA) {
    return false;
  }
  const canvas = els.canvas;
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
  if (!quiet) {
    setStatus(`Captured frame at ${formatTime(video.currentTime)}`);
  }
  syncVideoUi();
  return true;
}

function syncVideoUi() {
  const video = els.video;
  const hasVideo = Boolean(video.currentSrc);
  const hasFrameData = hasVideo && video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA;
  els.captureFrame.disabled = !hasFrameData;
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

function proxyUrl(url) {
  return `/api/proxy?url=${encodeURIComponent(url)}`;
}

function summarizePlayerResponse(response) {
  return {
    title: response?.playerResponse?.videoDetails?.title ?? response?.videoDetails?.title ?? null,
    formats: response?.playerResponse?.streamingData?.formats?.length ?? 0,
    adaptiveFormats: response?.playerResponse?.streamingData?.adaptiveFormats?.length ?? 0,
  };
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
    wasmReady = init({ module_or_path: "./pkg/av_ingest_extractor_bg.wasm" });
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
