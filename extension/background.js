// Orexa Downloader: browser side.
// - Catches downloads Chrome starts (like a click on a file link) and hands them to the
//   Orexa app, then removes Chrome's own copy. If Orexa is not available, Chrome keeps
//   the download, so nothing is ever lost.
// - Adds "Download with Orexa" to the right-click menu.
// - Talks to the app through the native connector "com.orexa.downloader".

const HOST = "com.orexa.downloader";

const DEFAULTS = {
  enabled: true,
  minSizeMB: 0, // 0 = capture every size
  onlyTypes: "", // e.g. "zip, exe, mp4"; empty = all types
  skipSites: "", // e.g. "example.com, intranet.local"
  showDialog: true, // show the "Download file info" window before anything starts
  sniff: true, // list the videos and audio a page plays, with a Download button
};
let cfg = { ...DEFAULTS };

async function loadConfig() {
  cfg = { ...DEFAULTS, ...(await chrome.storage.sync.get(DEFAULTS)) };
  cfg.showDialog = true; // the Download file info window is always shown
}
loadConfig();
chrome.storage.onChanged.addListener(loadConfig);

// ---------------------------------------------------------------------------
// Helpers (pure, tested in tests/logic.test.mjs)
// ---------------------------------------------------------------------------

function listFrom(text) {
  return String(text || "")
    .split(/[\s,;]+/)
    .map((s) => s.trim().toLowerCase().replace(/^\./, ""))
    .filter(Boolean);
}

function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return "";
  }
}

function extOf(nameOrUrl) {
  let s = String(nameOrUrl || "");
  try {
    if (/^https?:/i.test(s)) s = new URL(s).pathname;
  } catch {
    /* keep s */
  }
  s = s.split(/[\\/]/).pop() || "";
  const i = s.lastIndexOf(".");
  return i > 0 ? s.slice(i + 1).toLowerCase() : "";
}

function baseName(path) {
  return String(path || "").split(/[\\/]/).pop() || "";
}

function isHttp(url) {
  return /^https?:\/\//i.test(String(url || ""));
}

function isMagnet(url) {
  return /^magnet:\?/i.test(String(url || "").trim());
}

function isYoutube(url) {
  try {
    const h = new URL(url).hostname.toLowerCase();
    return h === "youtu.be" || h === "youtube.com" || h.endsWith(".youtube.com");
  } catch {
    return false;
  }
}

/** Cookies in the Netscape text format that yt-dlp reads. */
function toNetscape(cookies) {
  const lines = ["# Netscape HTTP Cookie File", "# Made by the Orexa Downloader extension. Keep this private."];
  for (const c of cookies) {
    const domain = c.domain || "";
    lines.push(
      [
        (c.httpOnly ? "#HttpOnly_" : "") + domain,
        domain.startsWith(".") ? "TRUE" : "FALSE",
        c.path || "/",
        c.secure ? "TRUE" : "FALSE",
        Math.floor(c.expirationDate || 0),
        c.name,
        c.value,
      ].join("\t")
    );
  }
  return lines.join("\n") + "\n";
}

/** Decide whether a Chrome download should be taken over by Orexa. */
function shouldCapture(item, c) {
  if (!c.enabled) return false;
  const url = item.finalUrl || item.url;
  if (!isHttp(url)) return false; // blob:, data:, file: cannot be handed over
  if (item.byExtensionId) return false; // started by an extension (including us)
  if (item.state && item.state !== "in_progress") return false;
  const host = hostOf(url);
  if (listFrom(c.skipSites).some((s) => host === s || host.endsWith("." + s))) return false;
  const types = listFrom(c.onlyTypes);
  if (types.length) {
    const ext = extOf(item.filename) || extOf(url);
    if (!types.includes(ext)) return false;
  }
  if (c.minSizeMB > 0 && item.fileSize > 0 && item.fileSize < c.minSizeMB * 1048576) return false;
  if (/^text\/html/i.test(item.mime || "") && !extOf(item.filename)) return false; // a page, not a file
  return true;
}

// ---------------------------------------------------------------------------
// Native connector
// ---------------------------------------------------------------------------

function sendNative(message) {
  return new Promise((resolve) => {
    try {
      chrome.runtime.sendNativeMessage(HOST, message, (response) => {
        if (chrome.runtime.lastError) {
          resolve({ ok: false, error: chrome.runtime.lastError.message });
        } else {
          resolve(response || { ok: false, error: "No answer from Orexa" });
        }
      });
    } catch (e) {
      resolve({ ok: false, error: String(e) });
    }
  });
}

async function cookieHeader(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return "";
  }
}

async function remember(entry) {
  const { recent = [] } = await chrome.storage.local.get("recent");
  recent.unshift({ ...entry, at: Date.now() });
  await chrome.storage.local.set({ recent: recent.slice(0, 8) });
}

// ---------------------------------------------------------------------------
// YouTube sign-in: YouTube often asks automated tools to prove they are not a bot. The
// browser is already signed in, so its YouTube cookies are shared with Orexa (kept on this
// computer only, in Orexa's own folder).
// ---------------------------------------------------------------------------

let lastCookieExport = 0;

async function exportYoutubeCookies(force) {
  if (!force && Date.now() - lastCookieExport < 10 * 60 * 1000) return false;
  try {
    const [yt, google] = await Promise.all([
      chrome.cookies.getAll({ domain: "youtube.com" }),
      chrome.cookies.getAll({ domain: "google.com" }),
    ]);
    const all = [...yt, ...google];
    if (!all.length) return false;
    const reply = await sendNative({ type: "cookies", site: "youtube", text: toNetscape(all) });
    if (reply.ok) lastCookieExport = Date.now();
    return !!reply.ok;
  } catch {
    return false;
  }
}

chrome.runtime.onStartup.addListener(() => exportYoutubeCookies(true));
chrome.runtime.onInstalled.addListener(() => {
  exportYoutubeCookies(true);
});

let badgeTimer;
function flashBadge(text, color) {
  chrome.action.setBadgeBackgroundColor({ color });
  chrome.action.setBadgeText({ text });
  clearTimeout(badgeTimer);
  badgeTimer = setTimeout(() => chrome.action.setBadgeText({ text: "" }), 2500);
}

function notifyProblem(message) {
  chrome.notifications.create({
    type: "basic",
    iconUrl: "icons/icon128.png",
    title: "Orexa Downloader",
    message,
  });
}

/** Send a link to Orexa. Returns true when Orexa accepted it. */
async function sendToOrexa({ url, filename, referrer, mime, size }) {
  // Fresh sign-in details first, so Orexa can read the video.
  if (isYoutube(url)) await exportYoutubeCookies(true);
  const cookie = await cookieHeader(url);
  const reply = await sendNative({
    type: "download",
    url,
    filename: filename || "",
    referrer: referrer || "",
    cookie,
    userAgent: navigator.userAgent,
    mime: mime || "",
    size: size > 0 ? size : 0,
  });
  if (reply.ok) {
    flashBadge("OK", "#22D3EE");
    remember({ url, filename: filename || url });
    return true;
  }
  flashBadge("!", "#B42318");
  return false;
}

// ---------------------------------------------------------------------------
// Capture browser downloads
// ---------------------------------------------------------------------------

// Chrome waits for every extension to answer before it picks a file name, and then shows its own
// "Save as" box if the user has "Ask where to save each file" on. For a download Orexa takes over,
// the answer is held back until we know what happens, so that box never appears: Chrome's copy is
// cancelled first. If Orexa cannot take it, the answer is given and Chrome carries on as usual.
const waitingForName = new Map(); // download id -> Chrome's callback

function answerFilename(id) {
  const suggest = waitingForName.get(id);
  if (!suggest) return;
  waitingForName.delete(id);
  try {
    suggest();
  } catch {
    /* the download is already gone */
  }
}

function onDeterminingFilename(item, suggest) {
  if (!shouldCapture(item, cfg)) {
    suggest(); // not ours: Chrome carries on right away
    return false;
  }
  waitingForName.set(item.id, suggest);
  const timer = setTimeout(() => answerFilename(item.id), 10 * 60 * 1000); // never keep Chrome waiting forever
  if (timer && timer.unref) timer.unref(); // (only matters in tests)
  handleDownload(item);
  return true; // we answer later
}
chrome.downloads.onDeterminingFilename.addListener(onDeterminingFilename);

async function handleDownload(item) {
  if (!shouldCapture(item, cfg)) return;
  // Hold Chrome's copy while the user decides.
  try {
    await chrome.downloads.pause(item.id);
  } catch {
    /* already finished or cannot pause: carry on */
  }
  const r = await requestDownload({
    url: item.finalUrl || item.url,
    filename: baseName(item.filename),
    referrer: item.referrer,
    mime: item.mime,
    size: item.fileSize,
    downloadId: item.id,
  });
  if (!r.ok) {
    // Orexa is not installed or not reachable: let Chrome finish the download itself.
    answerFilename(item.id);
    try {
      await chrome.downloads.resume(item.id);
    } catch {
      /* ignore */
    }
    return;
  }
  if (!r.dialog) await dropChromeDownload(item.id); // sent straight to Orexa
}

// ---------------------------------------------------------------------------
// The "Download file info" window
// ---------------------------------------------------------------------------

const DLG = "dlg:";

async function dropChromeDownload(id) {
  if (id === undefined || id === null) return;
  try {
    await chrome.downloads.cancel(id);
  } catch {
    /* already gone */
  }
  try {
    await chrome.downloads.erase({ id });
  } catch {
    /* ignore */
  }
  answerFilename(id); // only now: the download is cancelled, so Chrome has nothing to ask about
}

async function openDialog(data) {
  const token = crypto.randomUUID();
  const rec = { ...data, token, decided: false, windowId: null };
  await chrome.storage.session.set({ [DLG + token]: rec });
  let left;
  let top;
  try {
    const w = await chrome.windows.getLastFocused();
    if (w && w.width) {
      left = Math.max(0, Math.round((w.left || 0) + (w.width - 560) / 2));
      top = Math.max(0, Math.round((w.top || 0) + 60));
    }
  } catch {
    /* let Chrome place it */
  }
  const win = await chrome.windows.create({
    url: chrome.runtime.getURL("dialog.html?t=" + token),
    type: "popup",
    width: 560,
    height: 720,
    left,
    top,
    focused: true,
  });
  rec.windowId = win.id;
  await chrome.storage.session.set({ [DLG + token]: rec });
}

// Closing the window without choosing counts as Cancel.
chrome.windows.onRemoved.addListener(async (windowId) => {
  const all = await chrome.storage.session.get(null);
  for (const [key, rec] of Object.entries(all)) {
    if (!key.startsWith(DLG) || rec.windowId !== windowId) continue;
    if (!rec.decided) await dropChromeDownload(rec.downloadId);
    await chrome.storage.session.remove(key);
  }
});

/** Start the flow for a link: the dialog (or, if switched off, straight to Orexa). */
async function requestDownload(data) {
  const ping = await sendNative({ type: "ping" });
  if (!ping.ok) return { ok: false, dialog: false };
  data.kind = data.kind === "stream" ? "stream" : isMagnet(data.url) ? "torrent" : isYoutube(data.url) ? "youtube" : "file";
  if (cfg.showDialog) {
    await openDialog(data);
    return { ok: true, dialog: true };
  }
  return { ok: await sendToOrexa(data), dialog: false };
}

// ---------------------------------------------------------------------------
// Media sniffer: finds the video and audio files a page is playing, so they
// can be downloaded with one click. Pure helpers first (tested in tests/logic.test.mjs).
// ---------------------------------------------------------------------------

const MEDIA_EXTS = ["mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "mp3", "m4a", "ogg", "oga", "opus", "wav", "flac", "aac"];
const AUDIO_EXTS = ["mp3", "m4a", "ogg", "oga", "opus", "wav", "flac", "aac"];
const MIME_EXT = {
  "video/mp4": "mp4", "video/webm": "webm", "video/ogg": "ogv", "video/x-matroska": "mkv", "video/quicktime": "mov",
  "video/x-flv": "flv", "video/x-msvideo": "avi", "video/x-m4v": "m4v",
  "audio/mpeg": "mp3", "audio/mp3": "mp3", "audio/mp4": "m4a", "audio/x-m4a": "m4a", "audio/aac": "aac", "audio/ogg": "ogg",
  "audio/webm": "webm", "audio/wav": "wav", "audio/x-wav": "wav", "audio/flac": "flac", "audio/opus": "opus",
};
const MIN_MEDIA_BYTES = 300 * 1024; // smaller files are previews, sounds, ads
// YouTube streams come in pieces; the YouTube button handles those.
const SKIP_MEDIA_HOSTS = ["googlevideo.com", "youtube.com", "youtu.be"];

function headerValue(headers, name) {
  const h = (headers || []).find((x) => x.name && x.name.toLowerCase() === name);
  return h ? String(h.value || "") : "";
}

/** Total size from Content-Range ("bytes 0-99/5000") or Content-Length. */
function sizeFromHeaders(headers) {
  const cr = /\/(\d+)\s*$/.exec(headerValue(headers, "content-range"));
  if (cr) return Number(cr[1]);
  const len = Number(headerValue(headers, "content-length"));
  return len > 0 ? len : 0;
}

function nameFromDisposition(cd) {
  const star = /filename\*\s*=\s*utf-8''([^;]+)/i.exec(cd || "");
  if (star) {
    try {
      return decodeURIComponent(star[1].trim());
    } catch {
      /* fall through */
    }
  }
  const plain = /filename\s*=\s*"?([^";]+)"?/i.exec(cd || "");
  return plain ? plain[1].trim() : "";
}

/** Decide whether a response is a downloadable media file. Returns null if not. */
function sniffMedia(url, headers, statusCode) {
  if (!isHttp(url) || (statusCode && statusCode !== 200 && statusCode !== 206)) return null;
  const host = hostOf(url);
  if (SKIP_MEDIA_HOSTS.some((h) => host === h || host.endsWith("." + h))) return null;
  // Sites that play a video in small pieces (Facebook reels, for example) ask for each piece
  // by byte position. A piece is not a video you can save.
  if (/[?&](bytestart|byteend)=/i.test(url)) return null;
  const ct = headerValue(headers, "content-type").split(";")[0].trim().toLowerCase();
  if (/mp2t/.test(ct)) return null; // one piece of a stream
  const urlExt = extOf(url);
  // HLS / DASH playlists: kept as "stream" items; checked later by classifyManifest.
  const manifest = /mpegurl/.test(ct) || urlExt === "m3u8" ? "m3u8" : /dash\+xml/.test(ct) || urlExt === "mpd" ? "mpd" : "";
  if (manifest && !/^text\/html/.test(ct)) {
    let stem = "";
    try {
      stem = decodeURIComponent(baseName(new URL(url).pathname)).replace(/\.[a-z0-9]+$/i, "");
    } catch {
      /* no name */
    }
    return { url, ext: manifest, kind: "stream", size: 0, name: (stem || "video") + ".mp4" };
  }
  let ext = "";
  if (ct.startsWith("video/") || ct.startsWith("audio/")) {
    ext = MEDIA_EXTS.includes(urlExt) ? urlExt : MIME_EXT[ct] || (ct.startsWith("audio/") ? "m4a" : "mp4");
  } else if ((ct === "" || ct === "application/octet-stream" || ct === "binary/octet-stream") && MEDIA_EXTS.includes(urlExt)) {
    ext = urlExt;
  } else {
    return null;
  }
  const size = sizeFromHeaders(headers);
  if (size > 0 && size < MIN_MEDIA_BYTES) return null;
  let name = nameFromDisposition(headerValue(headers, "content-disposition"));
  if (!name) {
    try {
      name = decodeURIComponent(baseName(new URL(url).pathname));
    } catch {
      name = baseName(url);
    }
  }
  if (!name || !/\.[a-z0-9]{2,5}$/i.test(name)) name = (name || "media") + "." + ext;
  return { url, ext, kind: AUDIO_EXTS.includes(ext) ? "audio" : "video", size, name };
}

/** The same file requested in pieces (range=, bytestart=...) counts once. */
function mediaKey(url) {
  try {
    const u = new URL(url);
    for (const k of ["range", "bytestart", "byteend", "start", "end", "rn", "rbuf", "ts"]) u.searchParams.delete(k);
    u.hash = "";
    return u.toString();
  } catch {
    return url;
  }
}

/**
 * Look inside a playlist: only a main playlist (or a finished one) is worth listing; live
 * streams and copy-protected (DRM) ones are not. Returns { master } or null.
 */
function classifyManifest(text, ext) {
  const t = String(text || "").slice(0, 400000);
  if (ext === "m3u8") {
    if (!/^\s*#EXTM3U/.test(t)) return null;
    if (/#EXT-X-(?:SESSION-)?KEY:[^\n]*(?:SAMPLE-AES|skd:\/\/|com\.apple\.streamingkeydelivery)/i.test(t)) return null;
    if (/#EXT-X-STREAM-INF/.test(t)) return { master: true };
    if (/#EXTINF/.test(t) && /#EXT-X-ENDLIST/.test(t)) return { master: false };
    return null; // live or empty
  }
  if (!/<MPD[\s>]/.test(t) || /type\s*=\s*"dynamic"/.test(t)) return null;
  if (/<ContentProtection[^>]*(?:edef8ba9|9a04f079|widevine|playready)/i.test(t)) return null;
  return { master: true };
}

/**
 * A Facebook video page address (reel, watch, video, share, fb.watch), cleaned up, or "".
 * Facebook plays videos in pieces, so the page address is what Orexa downloads from.
 */
function facebookVideoUrl(url) {
  let u;
  try {
    u = new URL(url);
  } catch {
    return "";
  }
  const host = u.hostname.toLowerCase();
  const path = u.pathname;
  if (host === "fb.watch") return /^\/[\w-]{4,}/.test(path) ? u.origin + path : "";
  if (!(host === "facebook.com" || host.endsWith(".facebook.com"))) return "";
  let m = /^\/reels?\/(\d{6,})/.exec(path);
  if (m) return `https://www.facebook.com/reel/${m[1]}`;
  const v = u.searchParams.get("v");
  if (v && /^\d{6,}$/.test(v) && (/^\/watch\/?$/.test(path) || path === "/video.php")) return `https://www.facebook.com/watch/?v=${v}`;
  m = /\/videos\/(?:[^/]+\/)?(\d{6,})/.exec(path);
  if (m) return `https://www.facebook.com/watch/?v=${m[1]}`;
  if (/^\/share\/[rv]\/[\w-]+/.test(path)) return `https://www.facebook.com${path.replace(/\/?$/, "/")}`;
  return "";
}

/** The page title is a better file name when the file's own name is generic (video.mp4, 123.mp4). */
function friendlyMediaName(item, pageTitle) {
  const generic = /^(video\d*|media|index|master|main|manifest|chunklist|hls|playlist|stream|download|file|\d+|[a-f0-9-]{16,})\.[a-z0-9]+$/i;
  const title = String(pageTitle || "").replace(/[\\/:*?"<>|]+/g, " ").replace(/\s+/g, " ").trim().slice(0, 100);
  return generic.test(item.name) && title ? `${title}.${item.kind === "stream" ? "mp4" : item.ext}` : item.name;
}

/** Add an item to a tab's list (newest first, no repeats, at most 25). */
function addMedia(list, item, now) {
  const key = mediaKey(item.url);
  if (item.kind === "stream") {
    // A main playlist replaces the quality playlists the player loaded after it.
    if (item.master === false && list.some((x) => x.kind === "stream" && x.master)) return list;
    if (item.master) list = list.filter((x) => !(x.kind === "stream" && x.master === false));
  }
  const i = list.findIndex((x) => x.key === key);
  if (i >= 0) {
    if (item.size > list[i].size) list[i] = { ...list[i], size: item.size };
    return list;
  }
  return [{ ...item, key, at: now }, ...list].slice(0, 25);
}

const mediaKeyFor = (tabId) => "media:" + tabId;
let mediaChain = Promise.resolve();

// Files a page requests are only remembered as candidates (nothing is shown or checked). A
// candidate becomes a Download button when a video or audio on the page really plays.
const pendingKeyFor = (tabId) => "pending:" + tabId;
const PLAY_WINDOW_MS = 5 * 60 * 1000;

/** Which candidates belong to what just started playing. */
function selectPlayed(pending, src, now) {
  const key = isHttp(src) ? mediaKey(src) : "";
  if (key) {
    const exact = pending.filter((x) => x.key === key);
    if (exact.length) return exact; // a plain <video src>: it is exactly this file
  }
  // Streams (blob: sources) cannot be matched to a file: take what was requested lately.
  return pending.filter((x) => now - x.at <= PLAY_WINDOW_MS);
}

function onMediaHeaders(d) {
  if (!cfg.sniff || d.tabId < 0) return;
  const found = sniffMedia(d.url, d.responseHeaders, d.statusCode);
  if (!found) return;
  mediaChain = mediaChain
    .then(async () => {
      const k = pendingKeyFor(d.tabId);
      const got = await chrome.storage.session.get(k);
      const list = addMedia((got[k] || []).slice(), { ...found }, Date.now()).slice(0, 40);
      await chrome.storage.session.set({ [k]: list });
    })
    .catch(() => {});
}

/** A video or audio started playing in a tab: list the files that belong to it. */
function revealPlayed(tabId, src, pages) {
  if (!cfg.sniff || tabId == null || tabId < 0) return;
  mediaChain = mediaChain
    .then(async () => {
      const pk = pendingKeyFor(tabId);
      const mk = mediaKeyFor(tabId);
      const got = await chrome.storage.session.get([pk, mk]);
      const pending = got[pk] || [];
      // A Facebook video: list the video's page, which Orexa's video engine can download.
      const fb = (Array.isArray(pages) ? pages : []).map(facebookVideoUrl).find(Boolean);
      if (fb) {
        // Only the video playing now (moving from reel to reel replaces it, nothing piles up).
        let list = addMedia(
          [],
          { url: fb, ext: "mp4", kind: "page", size: 0, name: "Facebook video", pageUrl: fb },
          Date.now()
        );
        await chrome.storage.session.set({ [mk]: list });
        chrome.action.setBadgeBackgroundColor({ tabId, color: "#0B1220" });
        chrome.action.setBadgeText({ tabId, text: String(list.length) });
        chrome.tabs.sendMessage(tabId, { type: "media-update", items: list }, () => void chrome.runtime.lastError);
        prefetchPageVideo(fb); // read it now, so the window is ready when you click
        return;
      }
      const chosen = selectPlayed(pending, src, Date.now());
      if (!chosen.length) return;
      let tabInfo = null;
      try {
        tabInfo = await chrome.tabs.get(tabId);
      } catch {
        /* tab closed */
      }
      let list = []; // only what is playing now
      for (const found of chosen) {
        const item = { ...found };
        if (item.kind === "stream") {
          // Only now look inside the playlist (never for files nobody plays).
          let verdict = null;
          try {
            const resp = await fetch(item.url, { credentials: "include" });
            if (resp.ok) verdict = classifyManifest(await resp.text(), item.ext);
          } catch {
            /* cannot read it: do not list it */
          }
          if (!verdict) continue;
          item.master = verdict.master;
        }
        item.name = friendlyMediaName(item, tabInfo && tabInfo.title);
        item.pageUrl = (tabInfo && tabInfo.url) || "";
        list = addMedia(list, item, Date.now());
      }
      const chosenKeys = new Set(chosen.map((x) => x.key));
      await chrome.storage.session.set({
        [mk]: list,
        [pk]: pending.filter((x) => !chosenKeys.has(x.key)),
      });
      if (!list.length) return;
      chrome.action.setBadgeBackgroundColor({ tabId, color: "#0B1220" });
      chrome.action.setBadgeText({ tabId, text: String(list.length) });
      chrome.tabs.sendMessage(tabId, { type: "media-update", items: list }, () => void chrome.runtime.lastError);
    })
    .catch(() => {});
}

/** The video that was playing ended, was closed or scrolled away: its button goes away. */
function clearPlayed(tabId) {
  if (tabId == null || tabId < 0) return;
  mediaChain = mediaChain
    .then(async () => {
      await chrome.storage.session.remove(mediaKeyFor(tabId));
      chrome.action.setBadgeText({ tabId, text: "" });
      chrome.tabs.sendMessage(tabId, { type: "media-update", items: [] }, () => void chrome.runtime.lastError);
    })
    .catch(() => {});
}

function onNewPage(d) {
  if (d.tabId < 0) return;
  mediaChain = mediaChain
    .then(async () => {
      await chrome.storage.session.remove([mediaKeyFor(d.tabId), pendingKeyFor(d.tabId)]);
      chrome.action.setBadgeText({ tabId: d.tabId, text: "" });
    })
    .catch(() => {});
}

chrome.webRequest.onHeadersReceived.addListener(
  onMediaHeaders,
  { urls: ["http://*/*", "https://*/*"], types: ["media", "xmlhttprequest", "other"] },
  ["responseHeaders"]
);
chrome.webRequest.onBeforeRequest.addListener(onNewPage, { urls: ["http://*/*", "https://*/*"], types: ["main_frame"] });
chrome.tabs.onRemoved.addListener((tabId) => chrome.storage.session.remove([mediaKeyFor(tabId), pendingKeyFor(tabId)]));

const prefetched = new Map(); // video address -> when
async function prefetchYoutube(url) {
  const now = Date.now();
  if (now - (prefetched.get(url) || 0) < 20 * 60 * 1000) return;
  prefetched.set(url, now);
  if (prefetched.size > 50) prefetched.delete(prefetched.keys().next().value);
  await exportYoutubeCookies(false);
  sendNative({ type: "probe", url }); // Orexa keeps the answer for a while
}

/** Facebook (page) videos: have Orexa read the video as soon as it plays, like YouTube. */
async function prefetchPageVideo(url) {
  const now = Date.now();
  if (now - (prefetched.get(url) || 0) < 20 * 60 * 1000) return;
  prefetched.set(url, now);
  if (prefetched.size > 50) prefetched.delete(prefetched.keys().next().value);
  const cookie = await cookieHeader(url);
  sendNative({ type: "probe", kind: "stream", url, referrer: url, userAgent: navigator.userAgent, cookie });
}

async function mediaList(tabId) {
  const got = await chrome.storage.session.get(mediaKeyFor(tabId));
  return got[mediaKeyFor(tabId)] || [];
}

async function downloadMedia(tabId, key) {
  const item = (await mediaList(tabId)).find((x) => x.key === key);
  if (!item) return { ok: false };
  return requestDownload({
    url: item.url,
    // For a page entry Orexa reads the video's own title, so no file name is sent.
    filename: item.kind === "page" ? "" : item.name,
    referrer: item.pageUrl,
    size: item.size,
    kind: item.kind === "stream" || item.kind === "page" ? "stream" : undefined,
    mime: item.kind === "stream" || item.kind === "page" ? "" : (item.kind === "audio" ? "audio/" : "video/") + item.ext,
  });
}

// ---------------------------------------------------------------------------
// Right-click menu
// ---------------------------------------------------------------------------

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({ id: "orexa-link", title: "Download link with Orexa", contexts: ["link"] });
    chrome.contextMenus.create({ id: "orexa-media", title: "Download video or audio with Orexa", contexts: ["video", "audio"] });
    chrome.contextMenus.create({
      id: "orexa-page",
      title: "Download this video with Orexa",
      contexts: ["page"],
      documentUrlPatterns: ["https://*.youtube.com/*"],
    });
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const url = info.menuItemId === "orexa-link" ? info.linkUrl : info.menuItemId === "orexa-media" ? info.srcUrl : info.pageUrl;
  if (!isHttp(url) && !isMagnet(url)) {
    notifyProblem("That item cannot be sent to Orexa (it is not a web link).");
    return;
  }
  const r = await requestDownload({ url, referrer: tab && tab.url });
  if (!r.ok) notifyProblem("Could not reach Orexa Downloader. Open the app once, then try again.");
});

// ---------------------------------------------------------------------------
// Messages from the popup and the YouTube button
// ---------------------------------------------------------------------------

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  (async () => {
    if (msg.type === "send-url") {
      const r = await requestDownload({ url: msg.url, referrer: (sender.tab && sender.tab.url) || "" });
      sendResponse({ ok: r.ok });
    } else if (msg.type === "yt-prefetch") {
      // The page was opened: read the video now, so the dialog is ready when it is needed.
      if (cfg.sniff && isYoutube(msg.url)) prefetchYoutube(msg.url);
      sendResponse({ ok: true });
    } else if (msg.type === "media-stopped") {
      clearPlayed(sender.tab && sender.tab.id);
      sendResponse({ ok: true });
    } else if (msg.type === "media-played") {
      revealPlayed(sender.tab && sender.tab.id, msg.src || "", msg.pages);
      sendResponse({ ok: true });
    } else if (msg.type === "media-list") {
      const tabId = msg.tabId ?? (sender.tab && sender.tab.id);
      sendResponse({ ok: true, items: tabId == null ? [] : await mediaList(tabId) });
    } else if (msg.type === "media-download") {
      const tabId = msg.tabId ?? (sender.tab && sender.tab.id);
      const r = tabId == null ? { ok: false } : await downloadMedia(tabId, msg.key);
      sendResponse({ ok: !!r.ok });
    } else if (msg.type === "dialog-init") {
      const got = await chrome.storage.session.get(DLG + msg.token);
      sendResponse(got[DLG + msg.token] || null);
    } else if (msg.type === "dialog-done") {
      const got = await chrome.storage.session.get(DLG + msg.token);
      const rec = got[DLG + msg.token];
      if (rec) {
        rec.decided = true;
        await chrome.storage.session.set({ [DLG + msg.token]: rec });
        await dropChromeDownload(rec.downloadId); // Orexa has it, or the user cancelled
        if (msg.action !== "cancel") {
          flashBadge("OK", "#22D3EE");
          remember({ url: rec.url, filename: rec.filename || rec.url });
        }
      }
      sendResponse({ ok: true });
    } else if (msg.type === "dialog-chrome") {
      // Orexa could not take it: let Chrome finish the download after all.
      const got = await chrome.storage.session.get(DLG + msg.token);
      const rec = got[DLG + msg.token];
      if (rec) {
        rec.decided = true;
        await chrome.storage.session.set({ [DLG + msg.token]: rec });
        if (rec.downloadId !== undefined && rec.downloadId !== null) {
          answerFilename(rec.downloadId);
          try {
            await chrome.downloads.resume(rec.downloadId);
          } catch {
            /* ignore */
          }
        }
      }
      sendResponse({ ok: true });
    } else if (msg.type === "keepalive") {
      sendResponse({ ok: true }); // the open dialog pings now and then so this worker stays awake
    } else if (msg.type === "yt-cookies") {
      sendResponse({ ok: await exportYoutubeCookies(true) });
    } else if (msg.type === "yt-visit") {
      sendResponse({ ok: await exportYoutubeCookies(false) });
    } else if (msg.type === "ping") {
      sendResponse(await sendNative({ type: "ping" }));
    } else if (msg.type === "open") {
      sendResponse(await sendNative({ type: "open" }));
    } else {
      sendResponse({ ok: false, error: "unknown message" });
    }
  })();
  return true; // answer asynchronously
});

// Exported for tests (Node), ignored by Chrome.
if (typeof module !== "undefined") {
  module.exports = {
    shouldCapture, listFrom, extOf, baseName, hostOf, isHttp, isMagnet, isYoutube, toNetscape,
    sniffMedia, mediaKey, friendlyMediaName, addMedia, classifyManifest, selectPlayed, facebookVideoUrl,
    requestDownload, handleDownload, onDeterminingFilename, dropChromeDownload, setConfigForTests: (c) => { cfg = { ...cfg, ...c }; },
  };
}
