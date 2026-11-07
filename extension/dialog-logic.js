// Small pure helpers for the download dialog (tested in tests/dialog-logic.test.mjs).
(function (root) {
  const CATEGORIES = [
    { label: "Compressed", icon: "archive", exts: ["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso"] },
    { label: "Documents", icon: "doc", exts: ["pdf", "doc", "docx", "txt", "xls", "xlsx", "ppt", "pptx", "odt"] },
    { label: "Music", icon: "music", exts: ["mp3", "wav", "flac", "ogg", "m4a"] },
    { label: "Programs", icon: "box", exts: ["exe", "msi", "dmg", "apk", "deb"] },
    { label: "Video", icon: "film", exts: ["mp4", "mkv", "avi", "mov", "webm"] },
    { label: "Torrent", icon: "magnet", exts: ["torrent"] },
  ];

  function extOf(name) {
    const s = String(name || "").split(/[\\/]/).pop() || "";
    const i = s.lastIndexOf(".");
    return i > 0 ? s.slice(i + 1).toLowerCase() : "";
  }

  function categoryOf(name) {
    const ext = extOf(name);
    return CATEGORIES.find((c) => c.exts.includes(ext)) || { label: "Other", icon: "file", exts: [] };
  }

  function fmtSize(bytes) {
    const b = Number(bytes) || 0;
    if (b <= 0) return "";
    const KB = 1024, MB = KB * 1024, GB = MB * 1024;
    if (b >= GB) return (b / GB).toFixed(1) + " GB";
    if (b >= MB) return (b / MB).toFixed(1) + " MB";
    if (b >= KB) return Math.round(b / KB) + " KB";
    return b + " B";
  }

  function fmtClock(sec) {
    const s = Math.max(0, Math.floor(Number(sec) || 0));
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), r = s % 60;
    const two = (n) => String(n).padStart(2, "0");
    return h > 0 ? `${h}:${two(m)}:${two(r)}` : `${m}:${two(r)}`;
  }

  function cleanName(n) {
    return String(n || "")
      .split(/[\\/]/)
      .pop()
      .replace(/[:*?"<>|]/g, "_")
      .trim();
  }

  /** A file name from a link; signed links keep the real name in the query. */
  function nameFromUrl(url) {
    if (/^magnet:\?/i.test(String(url || "").trim())) {
      const m = /[?&]dn=([^&]+)/i.exec(url);
      try {
        return m ? cleanName(decodeURIComponent(m[1].replace(/\+/g, " "))) || "Torrent" : "Torrent";
      } catch {
        return "Torrent";
      }
    }
    try {
      const u = new URL(url);
      for (const k of ["response-content-disposition", "rscd", "content-disposition"]) {
        const v = u.searchParams.get(k);
        const m = v && /filename\*?=(?:UTF-8'')?"?([^";]+)"?/i.exec(v);
        if (m) return cleanName(decodeURIComponent(m[1]));
      }
      const q = u.searchParams.get("filename") || u.searchParams.get("file");
      if (q) return cleanName(q);
      const last = decodeURIComponent(u.pathname.split("/").pop() || "");
      return cleanName(last) || "download";
    } catch {
      return "download";
    }
  }

  function hostOf(url) {
    try {
      return new URL(url).hostname.replace(/^www\./, "");
    } catch {
      return "";
    }
  }

  /** The 11-character id of a YouTube video link, or "". */
  function youtubeId(url) {
    try {
      const u = new URL(url);
      let id = "";
      if (u.hostname === "youtu.be") id = u.pathname.slice(1).split("/")[0];
      else if (u.searchParams.get("v")) id = u.searchParams.get("v");
      else {
        const m = /\/(?:shorts|live|embed)\/([^/?#]+)/.exec(u.pathname);
        if (m) id = m[1];
      }
      return /^[\w-]{11}$/.test(id) ? id : "";
    } catch {
      return "";
    }
  }

  /** Quality buttons that can be shown before the video has been read. */
  function defaultChoices(hasFfmpeg) {
    const audio = { label: "Audio only", height: 0, audio: true, size: 0 };
    if (!hasFfmpeg) return [{ label: "Best available", height: 0, audio: false, size: 0 }, audio];
    return [1080, 720, 480, 360].map((h) => ({ label: h + "p", height: h, audio: false, size: 0 })).concat(audio);
  }

  /** After the video has been read, keep the user's pick if it is still offered. */
  function keepChoice(prev, list, fallback) {
    const i = prev ? list.findIndex((c) => c.audio === prev.audio && c.height === prev.height) : -1;
    return i >= 0 ? i : Number.isInteger(fallback) ? fallback : 0;
  }

  /** File name without its extension. */
  function stemOf(name) {
    const n = cleanName(name);
    const i = n.lastIndexOf(".");
    return i > 0 ? n.slice(0, i) : n;
  }

  /** Which kind of file this is, for choosing a folder: Video, Music, Compressed, Torrent... */
  function folderKey(name, { video = false, audio = false, torrent = false } = {}) {
    if (torrent) return "Torrent";
    if (video) return audio ? "Music" : "Video";
    return categoryOf(name).label;
  }

  /** A magnet link or a .torrent file. */
  function isTorrent(url, name) {
    return /^magnet:\?/i.test(String(url || "").trim()) || extOf(name) === "torrent" || /\.torrent$/i.test(String(url || "").split(/[?#]/)[0]);
  }

  const api = { folderKey, isTorrent, extOf, categoryOf, fmtSize, fmtClock, cleanName, nameFromUrl, hostOf, youtubeId, defaultChoices, keepChoice, stemOf };
  if (typeof module !== "undefined") module.exports = api;
  else root.OrexaLogic = api;
})(typeof self !== "undefined" ? self : this);
