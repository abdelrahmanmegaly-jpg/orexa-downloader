// The download dialog (opened by the extension when you click a download).
(() => {
  const L = OrexaLogic;
  const HOST = "com.orexa.downloader";
  const $ = (id) => document.getElementById(id);
  const token = new URLSearchParams(location.search).get("t");

  const ICONS = {
    archive: '<rect width="20" height="5" x="2" y="3" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8"/><path d="M10 12h4"/>',
    doc: '<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/>',
    file: '<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/>',
    music: '<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>',
    box: '<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>',
    film: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M7 3v18"/><path d="M3 7.5h4"/><path d="M3 12h18"/><path d="M3 16.5h4"/><path d="M17 3v18"/><path d="M17 7.5h4"/><path d="M17 16.5h4"/>',
    folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
    clock: '<circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/>',
    download: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" x2="12" y1="15" y2="3"/>',
    check: '<path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><path d="m9 11 3 3L22 4"/>',
    play: '<polygon points="6 3 20 12 6 21 6 3"/>',
    magnet: '<path d="m6 15-4-4 6.75-6.77a7.79 7.79 0 0 1 11 11L13 22l-4-4 6.39-6.36a2.14 2.14 0 0 0-3-3L6 15"/><path d="m5 8 4 4"/><path d="m12 15 4 4"/>',
  };
  const svg = (name, size = 20, fill = "none") =>
    `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="${fill}" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`;

  let data = null; // what the extension knows about this download
  let isYt = false; // the video layout (quality buttons): YouTube and HLS/DASH streams
  let isStream = false;
  let isTorrent = false;
  let acting = false;
  let yt = null; // video details from Orexa
  let chosen = 0;
  let sizeBytes = 0;
  let ready = false;

  function native(message) {
    return new Promise((resolve) => {
      try {
        chrome.runtime.sendNativeMessage(HOST, message, (reply) => {
          if (chrome.runtime.lastError) resolve({ ok: false, error: chrome.runtime.lastError.message });
          else resolve(reply || { ok: false, error: "No answer from Orexa" });
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

  function showError(text, offerChrome) {
    const el = $("error");
    el.textContent = text;
    el.hidden = false;
    $("chrome").hidden = !(offerChrome && data && data.downloadId != null);
    fit();
  }
  const hideError = () => {
    $("error").hidden = true;
  };

  function setBusy(busy) {
    for (const id of ["now", "queue", "change"]) $(id).disabled = busy || (!ready && id !== "change");
  }

  // ---- rendering ----------------------------------------------------------

  function pill(kind, text) {
    const p = document.createElement("span");
    p.className = "pill " + (kind || "");
    if (kind === "wait") p.innerHTML = '<span class="spinner"></span>';
    else if (kind === "warn") p.textContent = "";
    else p.innerHTML = svg("check", 14);
    p.appendChild(document.createTextNode(text));
    return p;
  }

  function renderFileCard(extraNote) {
    const name = $("saveAs").value || L.nameFromUrl(data.url);
    const cat = isTorrent ? L.categoryOf("x.torrent") : L.categoryOf(name);
    const card = $("card");
    card.textContent = "";
    const tile = document.createElement("div");
    tile.className = "tile";
    tile.innerHTML = svg(cat.icon, 26);
    const info = document.createElement("div");
    info.className = "info";
    const nm = document.createElement("div");
    nm.className = "name";
    nm.dir = "auto";
    nm.textContent = name;
    const meta = document.createElement("div");
    meta.className = "meta";
    const size = L.fmtSize(sizeBytes);
    const txt = document.createElement("span");
    txt.textContent = size ? `${cat.label}, ${size}` : cat.label;
    meta.appendChild(txt);
    meta.appendChild(extraNote || pill("", "Link is ready"));
    info.append(nm, meta);
    card.append(tile, info);
  }

  function renderYtCard(state, message) {
    const card = $("card");
    card.textContent = "";
    const th = document.createElement("div");
    th.className = "thumb";
    if (yt && yt.thumbnail) {
      const img = document.createElement("img");
      img.src = yt.thumbnail;
      img.alt = "";
      th.appendChild(img);
    }
    const play = document.createElement("span");
    play.className = "play";
    play.innerHTML = svg("play", 18, "currentColor");
    th.appendChild(play);
    const info = document.createElement("div");
    info.className = "info";
    const nm = document.createElement("div");
    nm.className = "name";
    nm.dir = "auto";
    const meta = document.createElement("div");
    meta.className = "meta";
    if (state === "ready" && yt) {
      nm.textContent = yt.title;
      const who = [yt.channel, yt.duration > 0 ? L.fmtClock(yt.duration) : ""].filter(Boolean).join(", ");
      const t = document.createElement("span");
      t.dir = "auto";
      t.textContent = who;
      meta.appendChild(t);
    } else if (state === "error") {
      nm.textContent = "Could not read this video";
      meta.appendChild(pill("warn", message || "Try again later"));
    } else {
      nm.textContent = "Reading video details...";
      meta.appendChild(pill("wait", "This can take a little while"));
    }
    info.append(nm, meta);
    card.append(th, info);
  }

  function renderChips() {
    const wrap = $("chips");
    wrap.textContent = "";
    yt.choices.forEach((c, i) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "chip";
      b.setAttribute("role", "radio");
      b.setAttribute("aria-checked", String(i === chosen));
      b.textContent = c.label;
      b.addEventListener("click", () => {
        chosen = i;
        sizeBytes = c.size || 0;
        renderChips();
      });
      wrap.appendChild(b);
    });
    const c = yt.choices[chosen];
    sizeBytes = c.size || 0;
    $("sizeLine").textContent = c.size
      ? `About ${L.fmtSize(c.size)}`
      : yt.provisional
        ? "Sizes show up in a moment"
        : "Size will show once it starts";
    fit();
  }

  // ---- window size --------------------------------------------------------

  // Size the window to the content. The content height is measured on #app itself
  // (not the document, whose height is never smaller than the window, which made
  // the window grow every time), and nothing changes unless the need really changed.
  let fitTimer;
  let lastContent = 0;
  function fit() {
    clearTimeout(fitTimer);
    fitTimer = setTimeout(() => {
      const content = Math.ceil($("app").getBoundingClientRect().height);
      if (Math.abs(content - lastContent) < 2) return;
      lastContent = content;
      const frame = window.outerHeight - window.innerHeight;
      const h = Math.min(Math.max(content + frame, 380), 900);
      chrome.windows.getCurrent((w) => {
        if (w && Math.abs((w.height || 0) - h) > 2) chrome.windows.update(w.id, { height: h });
      });
    }, 40);
  }

  // ---- actions ------------------------------------------------------------

  async function act(mode) {
    hideError();
    acting = true;
    setBusy(true);
    const message = {
      type: "download",
      url: data.url,
      filename: isYt ? "" : L.cleanName($("saveAs").value) || L.nameFromUrl(data.url),
      referrer: data.referrer || "",
      cookie: await cookieHeader(data.url),
      userAgent: navigator.userAgent,
      mime: data.mime || "",
      size: sizeBytes,
      mode,
      saveDir: $("saveTo").value.trim(),
    };
    if (isYt) {
      const c = yt.choices[chosen];
      message.yt = { id: isStream ? "" : yt.id, height: c.height, audio: c.audio };
      message.title = yt.title;
      message.thumbUrl = yt.thumbnail;
      message.size = c.size || 0;
    }
    const reply = await native(message);
    if (!reply.ok) {
      acting = false;
      setBusy(false);
      showError(reply.error || "Orexa Downloader did not accept the download.", true);
      return;
    }
    await chrome.runtime.sendMessage({ type: "dialog-done", token, action: mode });
    window.close();
  }

  async function cancel() {
    await chrome.runtime.sendMessage({ type: "dialog-done", token, action: "cancel" });
    window.close();
  }

  // ---- YouTube, quickly ---------------------------------------------------
  // Reading a video with yt-dlp takes several seconds. The title and picture come from
  // YouTube's small "oEmbed" answer at once, with ready-made quality buttons, so Download
  // works right away. The full read (real qualities and sizes) fills in when it is done.

  async function fetchOembed(url) {
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), 6000);
    try {
      const r = await fetch("https://www.youtube.com/oembed?format=json&url=" + encodeURIComponent(url), { signal: ctl.signal });
      return r.ok ? await r.json() : null;
    } catch {
      return null;
    } finally {
      clearTimeout(timer);
    }
  }

  async function youtubeFast(info) {
    chrome.runtime.sendMessage({ type: "yt-visit" }, () => void chrome.runtime.lastError); // refresh the sign-in copy
    const reading = native({ type: "probe", url: data.url }); // starts now; answers at once if pre-read
    const id = L.youtubeId(data.url);
    const quick = id ? await fetchOembed(data.url) : null;
    let provisional = false;
    if (quick && quick.title) {
      provisional = true;
      yt = {
        id,
        title: quick.title,
        channel: quick.author_name || "",
        duration: 0,
        thumbnail: quick.thumbnail_url || "",
        choices: L.defaultChoices(!!info.ffmpeg),
        provisional: true,
      };
      chosen = 0;
      renderYtCard("ready");
      $("quality").hidden = false;
      renderChips();
      ready = true;
      if (!acting) setBusy(false);
    }
    let reply = await reading;
    if (!reply.ok && /sign in|not a bot/i.test(reply.error || "")) {
      // YouTube wants the browser's sign-in: send a fresh copy and read again.
      await chrome.runtime.sendMessage({ type: "yt-cookies" });
      reply = await native({ type: "probe", url: data.url });
    }
    if (!reply.ok) {
      if (!provisional) {
        yt = null;
        renderYtCard("error", "");
        showError(reply.error || "YouTube did not answer.", true);
        return;
      }
      yt.provisional = false;
      renderChips();
      showError("Could not read the details (" + (reply.error || "no answer") + "). You can still try the download.", false);
      return;
    }
    const prev = provisional ? yt.choices[chosen] : null;
    yt = reply;
    chosen = provisional ? L.keepChoice(prev, reply.choices, reply.default) : Number.isInteger(reply.default) ? reply.default : 0;
    renderYtCard("ready");
    $("quality").hidden = false;
    renderChips();
  }

  // ---- start --------------------------------------------------------------

  async function main() {
    $("nameIcon").innerHTML = svg("file", 18);
    $("folderIcon").innerHTML = svg("folder", 18);
    $("noteIcon").innerHTML = svg("clock", 18);
    $("queueIcon").innerHTML = svg("clock", 18);
    $("nowIcon").innerHTML = svg("download", 18);

    $("now").addEventListener("click", () => act("now"));
    $("queue").addEventListener("click", () => act("queue"));
    $("cancel").addEventListener("click", cancel);
    $("chrome").addEventListener("click", async () => {
      await chrome.runtime.sendMessage({ type: "dialog-chrome", token });
      window.close();
    });
    $("change").addEventListener("click", async () => {
      const r = await native({ type: "pick-folder", start: $("saveTo").value });
      if (r.ok && r.path) $("saveTo").value = r.path;
    });
    $("saveAs").addEventListener("input", () => {
      if (!isYt) renderFileCard();
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") cancel();
      else if (e.key === "Enter" && ready && !e.target.matches("button")) act("now");
    });
    new ResizeObserver(fit).observe($("app"));
    // While this window is open, keep the extension's background worker awake (it is holding
    // the browser's download until you choose).
    setInterval(() => chrome.runtime.sendMessage({ type: "keepalive" }, () => void chrome.runtime.lastError), 20000);
    setBusy(true);

    data = await chrome.runtime.sendMessage({ type: "dialog-init", token });
    if (!data) {
      $("title").textContent = "This dialog has expired";
      $("card").textContent = "";
      showError("Close this window and click the download again.", false);
      return;
    }
    isStream = data.kind === "stream";
    isTorrent = !isStream && data.kind !== "youtube" && L.isTorrent(data.url, data.filename);
    isYt = data.kind === "youtube" || isStream;
    $("host").textContent = /^magnet:/i.test(data.url) ? "Magnet link" : L.hostOf(data.url);
    sizeBytes = data.size > 0 ? data.size : 0;

    if (isYt) {
      $("title").textContent = "Download video";
      $("subtitle").textContent = "Choose the quality, then start or queue it.";
      $("nameField").hidden = true;
      renderYtCard("wait");
    } else {
      $("saveAs").value = data.filename || L.nameFromUrl(data.url);
      if (isTorrent) {
        $("title").textContent = "Download torrent";
        $("subtitle").textContent = "Choose what to do with this torrent.";
        $("nameField").hidden = true; // a torrent is named by its own contents
      }
      renderFileCard(pill("wait", "Checking link"));
    }

    const info = await native({ type: "info" });
    if (!info.ok) {
      showError(
        "Orexa Downloader could not be reached. Open the app once, then try again.",
        true
      );
      if (!isYt) renderFileCard(pill("warn", "Not checked"));
      return;
    }
    $("saveTo").value = info.defaultDir || "";
    if (info.running > 0) {
      $("noteText").textContent = `${info.running} download${info.running === 1 ? " is" : "s are"} running. Choose Add to queue to start this one after ${info.running === 1 ? "it" : "them"}.`;
      $("note").hidden = false;
    }

    if (isStream) {
      // Show the video straight away with a "best quality" button, so Download works at once;
      // the real title and qualities fill in when Orexa has read the video (usually already done:
      // it starts reading when the video begins to play).
      const named = L.stemOf(data.filename || "");
      yt = {
        id: "",
        title: named || (/(^|\.)(facebook\.com|fb\.watch)$/.test(L.hostOf(data.url)) ? "Facebook video" : "Video"),
        channel: "",
        duration: 0,
        thumbnail: "",
        choices: [
          { label: "Best available", height: 0, audio: false, size: 0 },
          { label: "Audio only", height: 0, audio: true, size: 0 },
        ],
        provisional: true,
      };
      chosen = 0;
      renderYtCard("ready");
      $("quality").hidden = false;
      renderChips();
      ready = true;
      if (!acting) setBusy(false);
      const reply = await native({
        type: "probe",
        kind: "stream",
        url: data.url,
        referrer: data.referrer || "",
        userAgent: navigator.userAgent,
        cookie: await cookieHeader(data.url),
      });
      if (!reply.ok) {
        yt.provisional = false;
        renderChips();
        showError("Could not read the details (" + (reply.error || "no answer") + "). You can still try the download.", false);
      } else {
        const prev = yt.choices[chosen];
        reply.title = named || reply.title;
        reply.id = "";
        yt = reply;
        chosen = L.keepChoice(prev, reply.choices, reply.default);
        renderYtCard("ready");
        renderChips();
      }
    } else if (isYt) {
      await youtubeFast(info);
      if (!yt) return;
    } else {
      if (!sizeBytes && !/^magnet:/i.test(data.url)) {
        try {
          const head = await fetch(data.url, { method: "HEAD", credentials: "include" });
          const len = Number(head.headers.get("content-length"));
          if (len > 0) sizeBytes = len;
        } catch {
          /* size stays unknown */
        }
      }
      renderFileCard();
    }
    ready = true;
    if (!acting) setBusy(false);
    fit();
  }

  main();
})();
