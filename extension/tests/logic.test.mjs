// Run with:  node extension/tests/logic.test.mjs
import assert from "node:assert/strict";
import { createRequire } from "node:module";

// Minimal stand-in for the Chrome APIs the background script touches while loading.
const noop = { addListener() {} };
const calls = { created: [], paused: [], resumed: [], cancelled: [], sessionSet: [], native: [], downloaded: [] };
let handbackItems = []; // what the connector hands back when asked
let hostAnswers = true; // whether the connector "answers"
globalThis.chrome = {
  storage: {
    sync: { get: async (d) => d },
    local: { get: async () => ({}), set: async () => {} },
    session: { set: async (o) => calls.sessionSet.push(o), get: async () => ({}), remove: async () => {} },
    onChanged: noop,
  },
  downloads: {
    onDeterminingFilename: noop,
    pause: async (id) => calls.paused.push(id),
    resume: async (id) => calls.resumed.push(id),
    cancel: async (id) => calls.cancelled.push(id),
    erase: async () => {},
    download: async (opts) => { calls.downloaded.push(opts); return 1; },
  },
  windows: {
    onRemoved: noop,
    getLastFocused: async () => ({ left: 100, top: 50, width: 1200 }),
    create: async (opts) => {
      calls.created.push(opts);
      return { id: 77 };
    },
  },
  runtime: {
    onInstalled: noop, onStartup: noop, onMessage: noop,
    getURL: (p) => "chrome-extension://abc/" + p,
    lastError: null,
    sendNativeMessage(host, msg, cb) {
      calls.native.push(msg);
      if (msg.type === "handback") {
        const items = handbackItems;
        handbackItems = [];
        cb({ ok: true, items });
        return;
      }
      cb(hostAnswers ? { ok: true } : undefined);
    },
  },
  contextMenus: { onClicked: noop, removeAll() {}, create() {} },
  action: { setBadgeText() {}, setBadgeBackgroundColor() {} },
  notifications: { create() {} },
  cookies: { getAll: async () => [] },
  webRequest: { onHeadersReceived: noop, onBeforeRequest: noop },
  tabs: { onRemoved: noop, get: async () => ({}), sendMessage() {} },
};

const require = createRequire(import.meta.url);
const { shouldCapture, listFrom, extOf, baseName, hostOf, isHttp, isMagnet, isYoutube, toNetscape, requestDownload, handleDownload, onDeterminingFilename, dropChromeDownload, downloadInBrowser, takeBack, setConfigForTests, sniffMedia, mediaKey, friendlyMediaName, addMedia, classifyManifest, selectPlayed, facebookVideoUrl } = require("../background.js");

const base = { enabled: true, minSizeMB: 0, onlyTypes: "", skipSites: "" };
const dl = (over = {}) => ({
  url: "https://example.com/files/setup.exe",
  state: "in_progress",
  filename: "C:\\Users\\me\\Downloads\\setup.exe",
  fileSize: 50 * 1048576,
  mime: "application/octet-stream",
  ...over,
});

let n = 0;
const test = (name, fn) => {
  fn();
  n++;
  console.log("ok -", name);
};

test("captures an ordinary file download", () => assert.equal(shouldCapture(dl(), base), true));
test("does nothing when switched off", () => assert.equal(shouldCapture(dl(), { ...base, enabled: false }), false));
test("images stay with the browser unless switched on", () => {
  const png = dl({ url: "https://www.flaticon.com/download/icon/6422200?format=png", filename: "C:\\Users\\me\\Downloads\\6422200.png", mime: "image/png" });
  assert.equal(shouldCapture(png, base), false, "png by name and type");
  assert.equal(shouldCapture(dl({ url: "https://x.com/a.jpg", filename: "", mime: "" }), base), false, "jpg by address");
  assert.equal(shouldCapture(dl({ url: "https://x.com/get?id=1", filename: "", mime: "image/webp" }), base), false, "by type only");
  assert.equal(shouldCapture(png, { ...base, catchImages: true }), true, "caught when the option is on");
  assert.equal(shouldCapture(dl({ url: "https://x.com/photos.zip", filename: "photos.zip", mime: "application/zip" }), base), true, "a zip of photos is still caught");
});
test("ignores blob:, data: and file: links", () => {
  for (const u of ["blob:https://x/abc", "data:text/plain,hi", "file:///c:/a.zip"]) {
    assert.equal(shouldCapture(dl({ url: u }), base), false, u);
  }
});
test("uses the final address after redirects", () =>
  assert.equal(shouldCapture(dl({ url: "blob:x", finalUrl: "https://cdn.example.com/a.zip" }), base), true));
test("ignores downloads started by extensions", () => assert.equal(shouldCapture(dl({ byExtensionId: "abc" }), base), false));
test("ignores finished downloads", () => assert.equal(shouldCapture(dl({ state: "complete" }), base), false));
test("skips listed sites and their subdomains", () => {
  const c = { ...base, skipSites: "example.com" };
  assert.equal(shouldCapture(dl(), c), false);
  assert.equal(shouldCapture(dl({ url: "https://dl.example.com/a.zip" }), c), false);
  assert.equal(shouldCapture(dl({ url: "https://notexample.com/a.zip" }), c), true);
});
test("filters by file type", () => {
  const c = { ...base, onlyTypes: "zip, .iso" };
  assert.equal(shouldCapture(dl(), c), false);
  assert.equal(shouldCapture(dl({ filename: "C:\\d\\disk.ISO" }), c), true);
});
test("filters by minimum size, but captures when the size is unknown", () => {
  const c = { ...base, minSizeMB: 10 };
  assert.equal(shouldCapture(dl({ fileSize: 1048576 }), c), false);
  assert.equal(shouldCapture(dl({ fileSize: 20 * 1048576 }), c), true);
  assert.equal(shouldCapture(dl({ fileSize: -1 }), c), true);
});
test("does not take over plain web pages", () =>
  assert.equal(shouldCapture(dl({ filename: "", mime: "text/html; charset=utf-8" }), base), false));
test("helpers", () => {
  assert.deepEqual(listFrom(" Zip, .EXE ;mp4 "), ["zip", "exe", "mp4"]);
  assert.equal(extOf("https://a.com/x/file.tar.gz?token=1"), "gz");
  assert.equal(extOf("C:\\a\\b\\movie.MP4"), "mp4");
  assert.equal(extOf("noextension"), "");
  assert.equal(baseName("C:\\Users\\me\\Downloads\\a b.zip"), "a b.zip");
  assert.equal(hostOf("https://Sub.Example.com/x"), "sub.example.com");
  assert.equal(isHttp("ftp://x"), false);
});

test("recognises YouTube links", () => {
  assert.equal(isYoutube("https://www.youtube.com/watch?v=abc"), true);
  assert.equal(isYoutube("https://youtu.be/abc"), true);
  assert.equal(isYoutube("https://m.youtube.com/shorts/abc"), true);
  assert.equal(isYoutube("https://notyoutube.com/watch"), false);
  assert.equal(isYoutube("https://youtube.com.evil.example/"), false);
});
test("writes cookies in the Netscape format yt-dlp reads", () => {
  const text = toNetscape([
    { domain: ".youtube.com", path: "/", secure: true, httpOnly: true, expirationDate: 1893456000.5, name: "SID", value: "abc" },
    { domain: "accounts.google.com", path: "/", secure: false, httpOnly: false, name: "s", value: "1" },
  ]);
  const lines = text.trim().split("\n");
  assert.equal(lines[0], "# Netscape HTTP Cookie File");
  assert.equal(lines[2], "#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1893456000\tSID\tabc");
  assert.equal(lines[3], "accounts.google.com\tFALSE\t/\tFALSE\t0\ts\t1");
});

// ---- the media sniffer ----------------------------------------------------
const hdr = (o) => Object.entries(o).map(([name, value]) => ({ name, value }));
test("sniffer: finds an mp4 by its content type", () => {
  const f = sniffMedia("https://cdn.site.com/v/clip.mp4?token=1", hdr({ "Content-Type": "video/mp4", "Content-Length": "52428800" }), 200);
  assert.equal(f.ext, "mp4");
  assert.equal(f.kind, "video");
  assert.equal(f.size, 52428800);
  assert.equal(f.name, "clip.mp4");
});
test("sniffer: audio files are marked as audio", () => {
  const f = sniffMedia("https://x.com/a/track", hdr({ "Content-Type": "audio/mpeg", "Content-Length": "4000000" }), 200);
  assert.equal(f.kind, "audio");
  assert.equal(f.name, "track.mp3");
});
test("sniffer: reads the full size from Content-Range on partial answers", () => {
  const f = sniffMedia("https://x.com/v.webm", hdr({ "Content-Type": "video/webm", "Content-Range": "bytes 0-999/9000000", "Content-Length": "1000" }), 206);
  assert.equal(f.size, 9000000);
});
test("sniffer: ignores stream pieces, pages, tiny files and YouTube", () => {
  assert.equal(sniffMedia("https://x.com/seg1.ts", hdr({ "Content-Type": "video/mp2t" }), 200), null);
  assert.equal(sniffMedia("https://x.com/page", hdr({ "Content-Type": "text/html" }), 200), null);
  assert.equal(sniffMedia("https://x.com/ping.mp4", hdr({ "Content-Type": "video/mp4", "Content-Length": "2000" }), 200), null);
  assert.equal(sniffMedia("https://rr1.googlevideo.com/videoplayback?x=1", hdr({ "Content-Type": "video/mp4" }), 200), null);
  assert.equal(sniffMedia("https://x.com/v.mp4", hdr({ "Content-Type": "video/mp4" }), 404), null);
});
test("sniffer: HLS and DASH playlists become stream items named .mp4", () => {
  const h = sniffMedia("https://cdn.x.com/v/master.m3u8?t=1", hdr({ "Content-Type": "application/vnd.apple.mpegurl" }), 200);
  assert.equal(h.kind, "stream");
  assert.equal(h.ext, "m3u8");
  assert.equal(h.name, "master.mp4");
  const d = sniffMedia("https://cdn.x.com/v/stream.mpd", hdr({ "Content-Type": "application/dash+xml" }), 200);
  assert.equal(d.kind, "stream");
  assert.equal(d.ext, "mpd");
  // some servers send playlists as plain text
  assert.equal(sniffMedia("https://cdn.x.com/a/index.m3u8", hdr({ "Content-Type": "text/plain" }), 200).kind, "stream");
  assert.equal(friendlyMediaName(h, "Big Show: Ep 1"), "Big Show Ep 1.mp4");
});
test("manifests: main playlists and finished ones count, live and protected ones do not", () => {
  const master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=1280x720\nv.m3u8\n";
  const vod = "#EXTM3U\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
  const live = "#EXTM3U\n#EXTINF:4,\na.ts\n";
  const drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://key\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n";
  assert.deepEqual(classifyManifest(master, "m3u8"), { master: true });
  assert.deepEqual(classifyManifest(vod, "m3u8"), { master: false });
  assert.equal(classifyManifest(live, "m3u8"), null);
  assert.equal(classifyManifest(drm, "m3u8"), null);
  assert.equal(classifyManifest("<html>", "m3u8"), null);
  assert.deepEqual(classifyManifest('<?xml version="1.0"?><MPD type="static"></MPD>', "mpd"), { master: true });
  assert.equal(classifyManifest('<MPD type="dynamic"></MPD>', "mpd"), null);
  assert.equal(classifyManifest('<MPD type="static"><ContentProtection schemeIdUri="urn:uuid:EDEF8BA9-79D6-4ACE-A3C8-27DCD51D21ED"/></MPD>', "mpd"), null);
});
test("manifests: the main playlist replaces quality playlists, never the other way", () => {
  let list = [];
  list = addMedia(list, { url: "https://x.com/q720.m3u8", kind: "stream", master: false, size: 0, name: "q720.mp4" }, 1);
  list = addMedia(list, { url: "https://x.com/master.m3u8", kind: "stream", master: true, size: 0, name: "master.mp4" }, 2);
  assert.deepEqual(list.map((x) => x.name), ["master.mp4"]);
  list = addMedia(list, { url: "https://x.com/q360.m3u8", kind: "stream", master: false, size: 0, name: "q360.mp4" }, 3);
  assert.equal(list.length, 1);
});
test("played: a plain video file is matched exactly, streams by what was requested lately", () => {
  const now = 1_000_000;
  const pending = [
    { key: "https://x.com/a.mp4", url: "https://x.com/a.mp4", at: now - 10_000 },
    { key: "https://x.com/b.mp4", url: "https://x.com/b.mp4", at: now - 20_000 },
    { key: "https://x.com/old.mp4", url: "https://x.com/old.mp4", at: now - 20 * 60_000 },
  ];
  assert.deepEqual(selectPlayed(pending, "https://x.com/a.mp4?range=0-9", now).map((x) => x.key), ["https://x.com/a.mp4"]);
  // a blob: source cannot be matched to a file: only the recent ones, never the old preload
  assert.deepEqual(selectPlayed(pending, "blob:https://x.com/123", now).map((x) => x.key), ["https://x.com/a.mp4", "https://x.com/b.mp4"]);
  assert.deepEqual(selectPlayed(pending, "", now).length, 2);
  assert.deepEqual(selectPlayed([], "https://x.com/a.mp4", now), []);
});
test("sniffer: video pieces (bytestart/byteend) are never listed", () => {
  const piece = "https://video-cai1-1.xx.fbcdn.net/o1/v/t2/f2/m69/clip.mp4?bytestart=0&byteend=981000&efg=abc";
  assert.equal(sniffMedia(piece, hdr({ "Content-Type": "video/mp4", "Content-Length": "981000" }), 200), null);
});
test("facebook: reels, watch, videos and share links become one clean page address", () => {
  assert.equal(facebookVideoUrl("https://www.facebook.com/reel/1234567890123?s=single_unit&fs=e"), "https://www.facebook.com/reel/1234567890123");
  assert.equal(facebookVideoUrl("https://m.facebook.com/reels/1234567890123/"), "https://www.facebook.com/reel/1234567890123");
  assert.equal(facebookVideoUrl("https://www.facebook.com/watch/?v=9876543210&t=5"), "https://www.facebook.com/watch/?v=9876543210");
  assert.equal(facebookVideoUrl("https://www.facebook.com/video.php?v=9876543210"), "https://www.facebook.com/watch/?v=9876543210");
  assert.equal(facebookVideoUrl("https://www.facebook.com/SomePage/videos/1112223334445/"), "https://www.facebook.com/watch/?v=1112223334445");
  assert.equal(facebookVideoUrl("https://www.facebook.com/share/r/1AbCdEf2Gh/"), "https://www.facebook.com/share/r/1AbCdEf2Gh/");
  assert.equal(facebookVideoUrl("https://fb.watch/xYz12AbC/"), "https://fb.watch/xYz12AbC/");
  for (const bad of ["https://www.facebook.com/", "https://www.facebook.com/SomeUser", "https://example.com/reel/1234567890123", "https://www.facebook.com/watch/", "not a link"]) {
    assert.equal(facebookVideoUrl(bad), "", bad);
  }
});
test("sniffer: octet-stream counts only when the address ends in a media type", () => {
  assert.equal(sniffMedia("https://x.com/get?id=1", hdr({ "Content-Type": "application/octet-stream" }), 200), null);
  assert.equal(sniffMedia("https://x.com/movie.mkv", hdr({ "Content-Type": "application/octet-stream" }), 200).ext, "mkv");
});
test("sniffer: uses the file name the server gives", () => {
  const f = sniffMedia("https://x.com/dl?id=9", hdr({ "Content-Type": "video/mp4", "Content-Disposition": "attachment; filename=\"My Trip.mp4\"" }), 200);
  assert.equal(f.name, "My Trip.mp4");
});
test("sniffer: the page title replaces a generic name", () => {
  assert.equal(friendlyMediaName({ name: "video.mp4", ext: "mp4" }, "Cool: Clip?"), "Cool Clip.mp4");
  assert.equal(friendlyMediaName({ name: "holiday.mp4", ext: "mp4" }, "Cool Clip"), "holiday.mp4");
});
test("sniffer: pieces of one file are listed once, newest first", () => {
  assert.equal(mediaKey("https://x.com/v.mp4?range=0-99&a=1"), mediaKey("https://x.com/v.mp4?a=1&range=100-199"));
  let list = [];
  list = addMedia(list, { url: "https://x.com/v.mp4?range=0-9", size: 10, name: "v.mp4" }, 1);
  list = addMedia(list, { url: "https://x.com/v.mp4?range=10-19", size: 5000000, name: "v.mp4" }, 2);
  list = addMedia(list, { url: "https://x.com/w.mp4", size: 1, name: "w.mp4" }, 3);
  assert.equal(list.length, 2);
  assert.equal(list[0].name, "w.mp4");
  assert.equal(list[1].size, 5000000);
});

// ---- the dialog flow ------------------------------------------------------
const flow = async (name, fn) => {
  await fn();
  n++;
  console.log("ok -", name);
};
await flow("a captured download opens the dialog and holds Chrome's copy", async () => {
  hostAnswers = true;
  setConfigForTests({ enabled: true, showDialog: true });
  calls.created.length = 0; calls.paused.length = 0; calls.sessionSet.length = 0;
  await handleDownload(dl({ id: 41, url: "https://example.com/big.iso", filename: "C:\\d\\big.iso" }));
  assert.deepEqual(calls.paused, [41]);
  assert.equal(calls.created.length, 1);
  assert.match(calls.created[0].url, /^chrome-extension:\/\/abc\/dialog\.html\?t=/);
  assert.equal(calls.created[0].type, "popup");
  const stored = Object.values(calls.sessionSet[0])[0];
  assert.equal(stored.downloadId, 41);
  assert.equal(stored.kind, "file");
  assert.equal(stored.decided, false);
});
await flow("a YouTube link is marked as a video", async () => {
  calls.created.length = 0; calls.sessionSet.length = 0;
  const r = await requestDownload({ url: "https://www.youtube.com/watch?v=abc" });
  assert.equal(r.ok, true);
  assert.equal(Object.values(calls.sessionSet[0])[0].kind, "youtube");
});
await flow("Chrome's Save-as box is held back while Orexa's window is open, and never shown if Orexa takes it", async () => {
  hostAnswers = true;
  setConfigForTests({ enabled: true, showDialog: true });
  const answered = [];
  const tick = () => new Promise((r) => setTimeout(r, 30));
  const ret = onDeterminingFilename(dl({ id: 60, url: "https://example.com/p.zip" }), () => answered.push(60));
  await tick();
  assert.equal(ret, true, "Chrome is told we answer later");
  assert.deepEqual(answered, [], "Chrome has not been answered, so it cannot show Save as");
  await dropChromeDownload(60); // Orexa took it: Chrome's copy is cancelled first, then released
  assert.deepEqual(answered, [60]);
});
await flow("a download we do not take is answered at once", async () => {
  const answered = [];
  const ret = onDeterminingFilename(dl({ id: 61, url: "blob:https://x/abc" }), () => answered.push(61));
  assert.equal(ret, false);
  assert.deepEqual(answered, [61]);
});
await flow("if Orexa cannot be reached, Chrome is answered and carries on", async () => {
  hostAnswers = false;
  const answered = [];
  const ret = onDeterminingFilename(dl({ id: 62, url: "https://example.com/q.zip" }), () => answered.push(62));
  assert.equal(ret, true);
  await new Promise((r) => setTimeout(r, 60));
  assert.deepEqual(answered, [62]);
  hostAnswers = true;
});
await flow("a magnet link opens the dialog as a torrent", async () => {
  hostAnswers = true;
  calls.created.length = 0; calls.sessionSet.length = 0;
  const r = await requestDownload({ url: "magnet:?xt=urn:btih:abc&dn=Sintel" });
  assert.equal(r.ok, true);
  assert.equal(isMagnet("MAGNET:?xt=urn:btih:abc"), true);
  assert.equal(isMagnet("https://x.com/a"), false);
  assert.equal(Object.values(calls.sessionSet[0])[0].kind, "torrent");
});
await flow("if Orexa cannot be reached, Chrome keeps and finishes the download", async () => {
  hostAnswers = false;
  calls.created.length = 0; calls.resumed.length = 0;
  await handleDownload(dl({ id: 42 }));
  assert.equal(calls.created.length, 0, "no dialog when Orexa is unavailable");
  assert.deepEqual(calls.resumed, [42]);
  hostAnswers = true;
});
await flow("with the dialog switched off, the link goes straight to Orexa and Chrome's copy is dropped a little later", async () => {
  setConfigForTests({ showDialog: false });
  calls.created.length = 0; calls.native.length = 0; calls.cancelled.length = 0;
  await handleDownload(dl({ id: 43, url: "https://example.com/a.zip" }));
  assert.equal(calls.created.length, 0);
  assert.ok(calls.native.some((m) => m.type === "download" && m.url.endsWith("a.zip")));
  assert.deepEqual(calls.cancelled, [], "kept for a few seconds in case the site refuses Orexa");
  await new Promise((r) => setTimeout(r, 10500));
  assert.deepEqual(calls.cancelled, [43], "then dropped: Orexa has it");
  setConfigForTests({ showDialog: true });
});

await flow("a link the site refused Orexa is downloaded by the browser itself", async () => {
  calls.downloaded.length = 0;
  await downloadInBrowser({ url: "https://www.flaticon.com/download/icon/1?x=1", filename: "icon.png" });
  assert.deepEqual(calls.downloaded[0], { url: "https://www.flaticon.com/download/icon/1?x=1", conflictAction: "uniquify", filename: "icon.png" });
  calls.downloaded.length = 0;
  await downloadInBrowser({ url: "https://a.com/f", filename: "bad:name?.zip" });
  assert.equal(calls.downloaded[0].filename, undefined, "a name Windows cannot use is left to Chrome");
  calls.downloaded.length = 0;
  await downloadInBrowser({ url: "javascript:alert(1)" });
  assert.equal(calls.downloaded.length, 0, "only web links");
});
await flow("when the site refuses Orexa, Chrome's own paused download simply carries on", async () => {
  setConfigForTests({ showDialog: false });
  calls.downloaded.length = 0; calls.native.length = 0; calls.resumed.length = 0; calls.cancelled.length = 0;
  handbackItems = [{ id: 7, url: "https://site.example/only-browsers.zip", filename: "only-browsers.zip" }];
  await handleDownload(dl({ id: 44, url: "https://site.example/only-browsers.zip" }));
  await new Promise((r) => setTimeout(r, 2000));
  assert.ok(calls.native.some((m) => m.type === "handback"), "asked for hand-backs");
  assert.deepEqual(calls.resumed, [44], "the same download, not a new one");
  assert.equal(calls.downloaded.length, 0, "no second download (no second dialog)");
  await new Promise((r) => setTimeout(r, 9000));
  assert.deepEqual(calls.cancelled, [], "and it is not dropped later");
  setConfigForTests({ showDialog: true });
});
await flow("a hand-back with no Chrome copy (sent from the menu) starts a browser download", async () => {
  calls.downloaded.length = 0;
  await takeBack({ id: 8, url: "https://site.example/from-menu.zip", filename: "from-menu.zip" });
  assert.equal(calls.downloaded[0].url, "https://site.example/from-menu.zip");
});
await flow("a download this extension started is never captured again", async () => {
  await downloadInBrowser({ url: "https://site.example/again.zip" });
  assert.equal(shouldCapture(dl({ url: "https://site.example/again.zip" }), base), false);
});

console.log(`\n${n} tests passed`);
process.exit(0); // the hand-back watch keeps a timer going
