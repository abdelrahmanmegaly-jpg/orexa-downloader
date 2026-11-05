// Shows a small "Download video" button on pages that play video or audio files.
// The background script finds the files (see sniffMedia in background.js); this only draws
// the button and sends the choice back.
// Tell the extension when a video or audio is really playing (from any frame). Only then does
// a file found on the page become a Download button.
(() => {
  if (window.__orexaPlay) return;
  window.__orexaPlay = true;
  let activeEl = null; // the video or audio being played now (what the button is for)
  let activeKey = "";

  // Not a preview, thumbnail clip or muted background loop: a real, watchable video or audio.
  function real(v) {
    if (v.loop && v.muted) return false;
    const d = v.duration;
    if (!isFinite(d) || d < 15) return false; // live streams, short clips
    if (v.tagName === "VIDEO") {
      const r = v.getBoundingClientRect();
      if (r.width < 240 || r.height < 135) return false;
    }
    return true;
  }

  function check(v) {
    if (!(v instanceof HTMLMediaElement) || v.paused || v.ended || v.currentTime < 0.5) return;
    if (!real(v)) return;
    const src = v.currentSrc || v.src || "";
    // Sites that play a video in pieces (Facebook) are downloaded from the video's page, so
    // send the page address and the video links around the player; the extension picks the right one.
    const pages = [location.href];
    let el = v;
    for (let i = 0; el && i < 12 && pages.length < 6; i++, el = el.parentElement) {
      if (el.tagName === "A" && el.href) pages.push(el.href);
      const a = el.querySelector && el.querySelector('a[href*="/reel/"], a[href*="/videos/"], a[href*="watch/?v="], a[href*="/share/"]');
      if (a && a.href) pages.push(a.href);
    }
    const key = src + "|" + pages[0];
    if (v === activeEl && key === activeKey) return; // still the same one
    activeEl = v;
    activeKey = key;
    chrome.runtime.sendMessage({ type: "media-played", src, pages }, () => void chrome.runtime.lastError);
  }

  // When what was playing ends, is removed from the page, or is paused and out of view (for
  // example you scrolled to the next reel), forget it: nothing is collected or kept.
  function stopped() {
    if (!activeEl) return;
    activeEl = null;
    activeKey = "";
    chrome.runtime.sendMessage({ type: "media-stopped" }, () => void chrome.runtime.lastError);
  }
  function inView(v) {
    const r = v.getBoundingClientRect();
    const h = Math.min(r.bottom, innerHeight) - Math.max(r.top, 0);
    return r.height > 0 && h >= r.height * 0.4;
  }
  setInterval(() => {
    if (!activeEl) return;
    if (!activeEl.isConnected || activeEl.ended || (activeEl.paused && !inView(activeEl))) stopped();
  }, 1000);
  document.addEventListener("ended", (e) => e.target === activeEl && stopped(), true);

  // Media events do not bubble, so listen while they travel down.
  document.addEventListener("playing", (e) => setTimeout(() => check(e.target), 1500), true);
  for (const v of document.querySelectorAll("video, audio")) setTimeout(() => check(v), 1500); // already playing
})();

(() => {
  if (window.top !== window || window.__orexaMedia) return;
  window.__orexaMedia = true;

  // Magnet links (torrents) open Orexa's window instead of a "choose an app" box.
  document.addEventListener(
    "click",
    (e) => {
      if (e.defaultPrevented || e.button !== 0) return;
      const a = e.target && e.target.closest ? e.target.closest('a[href^="magnet:" i]') : null;
      if (!a) return;
      e.preventDefault();
      const href = a.href;
      chrome.runtime.sendMessage({ type: "send-url", url: href }, (r) => {
        void chrome.runtime.lastError;
        if (!r || !r.ok) location.href = href; // Orexa is not reachable: let the browser handle it
      });
    },
    true
  );

  let items = [];
  let host = null;
  let root = null;
  let open = false;

  const STYLE = `
    :host { all: initial; }
    .wrap { position: fixed; right: 20px; bottom: 24px; z-index: 2147483647; display: flex; flex-direction: column; align-items: flex-end; gap: 8px;
      font: 500 13px/1.3 "Segoe UI", Roboto, Arial, sans-serif; color: #0b1220; }
    button { font: inherit; cursor: pointer; }
    .pill { display: flex; align-items: center; gap: 8px; padding: 11px 16px; border: 0; border-radius: 12px; background: #22d3ee; color: #0b1220;
      font-weight: 600; font-size: 14px; box-shadow: 0 6px 20px rgba(11,18,32,.35); }
    .pill:hover { background: #1fc0da; }
    .count { min-width: 20px; padding: 2px 6px; border-radius: 10px; background: #0b1220; color: #fff; font-size: 12px; text-align: center; }
    .menu { width: 320px; max-height: 320px; overflow: auto; padding: 6px; border-radius: 14px; background: #fff; box-shadow: 0 10px 30px rgba(11,18,32,.35); border: 1px solid #e2e8f0; }
    .item { display: flex; align-items: center; gap: 10px; width: 100%; padding: 9px 10px; border: 0; border-radius: 10px; background: transparent; text-align: left; color: #0b1220; }
    .item:hover { background: #f1f5f9; }
    .tag { flex: none; padding: 3px 7px; border-radius: 7px; background: #cffafe; font-size: 11px; font-weight: 700; text-transform: uppercase; }
    .name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .size { flex: none; color: #60717e; font-size: 12px; }
    .head { padding: 6px 10px 4px; color: #60717e; font-size: 12px; font-weight: 600; }
    .wrap.onvideo { flex-direction: column-reverse; }
    .wrap.onvideo .pill { padding: 9px 14px; font-size: 13px; }
    [hidden] { display: none !important; }
  `;

  const fmtSize = (n) => {
    if (!n) return "";
    const u = ["B", "KB", "MB", "GB"];
    let i = 0;
    let v = n;
    while (v >= 1024 && i < u.length - 1) {
      v /= 1024;
      i++;
    }
    return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${u[i]}`;
  };

  // The biggest video the person can see; the button sits on its top-right corner.
  function bestVideo() {
    let best = null;
    let area = 0;
    for (const v of document.querySelectorAll("video")) {
      const r = v.getBoundingClientRect();
      const w = Math.min(r.right, innerWidth) - Math.max(r.left, 0);
      const h = Math.min(r.bottom, innerHeight) - Math.max(r.top, 0);
      if (w < 200 || h < 110) continue;
      const cs = getComputedStyle(v);
      if (cs.visibility === "hidden" || cs.display === "none") continue;
      if (w * h > area) {
        area = w * h;
        best = r;
      }
    }
    return best;
  }

  function place() {
    if (!host) return;
    const wrap = root.querySelector(".wrap");
    const r = bestVideo();
    if (r) {
      const top = Math.min(Math.max(r.top, 0) + 12, innerHeight - 60);
      const right = Math.max(innerWidth - Math.min(r.right, innerWidth) + 12, 8);
      wrap.classList.add("onvideo");
      Object.assign(wrap.style, { top: top + "px", right: right + "px", bottom: "auto" });
      host.dataset.place = "video";
    } else {
      wrap.classList.remove("onvideo");
      Object.assign(wrap.style, { top: "auto", right: "20px", bottom: "24px" });
      host.dataset.place = "corner";
    }
  }
  let placeTimer = 0;
  const placeSoon = () => {
    if (!placeTimer) placeTimer = requestAnimationFrame(() => ((placeTimer = 0), place()));
  };
  addEventListener("scroll", placeSoon, true);
  addEventListener("resize", placeSoon);
  setInterval(place, 500);

  function build() {
    host = document.createElement("div");
    host.id = "orexa-media-host";
    root = host.attachShadow({ mode: "closed" });
    root.innerHTML = `<style>${STYLE}</style>
      <div class="wrap">
        <div class="menu" hidden></div>
        <button class="pill" type="button">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" x2="12" y1="15" y2="3"/></svg>
          <span class="label">Download video</span><span class="count">0</span>
        </button>
      </div>`;
    root.querySelector(".pill").addEventListener("click", () => {
      if (items.length === 1) send(items[0]);
      else {
        open = !open;
        render();
      }
    });
    document.addEventListener("click", (e) => {
      if (open && !e.composedPath().includes(host)) {
        open = false;
        render();
      }
    });
    document.documentElement.appendChild(host);
    place();
  }

  function send(item) {
    chrome.runtime.sendMessage({ type: "media-download", key: item.key }, (r) => {
      void chrome.runtime.lastError;
      const label = root.querySelector(".label");
      label.textContent = r && r.ok ? "Sent to Orexa" : "Open Orexa first";
      setTimeout(render, 2200);
    });
    open = false;
  }

  function render() {
    if (!items.length) {
      if (host) {
        host.remove();
        host = null;
        root = null;
      }
      return;
    }
    if (!host) build();
    const audioOnly = items.every((i) => i.kind === "audio");
    root.querySelector(".label").textContent = items.length === 1 ? (audioOnly ? "Download audio" : "Download video") : "Download media";
    root.querySelector(".count").textContent = String(items.length);
    root.querySelector(".count").hidden = items.length === 1;
    place();
    const menu = root.querySelector(".menu");
    menu.hidden = !open;
    menu.textContent = "";
    if (open) {
      const head = document.createElement("div");
      head.className = "head";
      head.textContent = "Playing on this page";
      menu.appendChild(head);
      for (const it of items) {
        const b = document.createElement("button");
        b.type = "button";
        b.className = "item";
        const tag = document.createElement("span");
        tag.className = "tag";
        tag.textContent = it.kind === "stream" ? (it.ext === "mpd" ? "DASH" : "HLS") : it.ext;
        const nm = document.createElement("span");
        nm.className = "name";
        nm.dir = "auto";
        nm.textContent = it.name;
        nm.title = it.name;
        const sz = document.createElement("span");
        sz.className = "size";
        sz.textContent = fmtSize(it.size);
        b.append(tag, nm, sz);
        b.addEventListener("click", () => send(it));
        menu.appendChild(b);
      }
    }
  }

  chrome.runtime.onMessage.addListener((msg) => {
    if (msg && msg.type === "media-update") {
      items = msg.items || [];
      render();
    }
  });
  chrome.runtime.sendMessage({ type: "media-list" }, (r) => {
    void chrome.runtime.lastError;
    if (r && r.ok && r.items && r.items.length) {
      items = r.items;
      render();
    }
  });
})();
