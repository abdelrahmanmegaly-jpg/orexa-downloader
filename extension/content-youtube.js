// Adds a "Download with Orexa" button on YouTube video pages.
(() => {
  const ID = "orexa-yt-button";

  const isVideoPage = () => {
    const p = location.pathname;
    return p === "/watch" || p.startsWith("/shorts/") || p.startsWith("/live/");
  };

  function toast(button, text, bad) {
    const label = button.querySelector("span");
    const old = label.textContent;
    label.textContent = text;
    button.style.background = bad ? "#B42318" : "#22D3EE";
    button.style.color = bad ? "#fff" : "#0B1220";
    setTimeout(() => {
      label.textContent = old;
      button.style.background = "#22D3EE";
      button.style.color = "#0B1220";
    }, 2200);
  }

  function build() {
    const b = document.createElement("button");
    b.id = ID;
    b.type = "button";
    b.innerHTML =
      '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
      '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" x2="12" y1="15" y2="3"/></svg>' +
      "<span>Download with Orexa</span>";
    Object.assign(b.style, {
      position: "fixed",
      right: "20px",
      bottom: "24px",
      zIndex: "2147483647",
      display: "flex",
      alignItems: "center",
      gap: "8px",
      padding: "11px 16px",
      border: "none",
      borderRadius: "12px",
      background: "#22D3EE",
      color: "#0B1220",
      font: "600 14px/1 'Segoe UI', Roboto, Arial, sans-serif",
      cursor: "pointer",
      boxShadow: "0 6px 20px rgba(11,18,32,.35)",
    });
    b.addEventListener("click", () => {
      chrome.runtime.sendMessage({ type: "send-url", url: location.href }, (resp) => {
        if (chrome.runtime.lastError || !resp || !resp.ok) toast(b, "Open Orexa first", true);
        else toast(b, "Sent to Orexa", false);
      });
    });
    return b;
  }

  let readFor = "";
  // Sit on the player's top-right corner (it stays in the page corner if no player is visible).
  function place() {
    const b = document.getElementById(ID);
    if (!b) return;
    const el = document.querySelector("#movie_player video, #movie_player") || document.querySelector("video");
    const r = el && el.getBoundingClientRect();
    if (r && r.width > 200 && r.height > 110 && r.bottom > 0 && r.top < innerHeight) {
      b.style.top = Math.min(Math.max(r.top, 0) + 12, innerHeight - 60) + "px";
      b.style.right = Math.max(innerWidth - Math.min(r.right, innerWidth) + 12, 8) + "px";
      b.style.bottom = "auto";
      b.dataset.place = "video";
    } else {
      b.style.top = "auto";
      b.style.right = "20px";
      b.style.bottom = "24px";
      b.dataset.place = "corner";
    }
  }

  function ensure() {
    const existing = document.getElementById(ID);
    if (isVideoPage()) {
      if (!existing) document.documentElement.appendChild(build());
      // Let Orexa read this video in the background, so Download is quick later.
      if (readFor !== location.href && document.visibilityState === "visible") {
        readFor = location.href;
        chrome.runtime.sendMessage({ type: "yt-prefetch", url: location.href }, () => void chrome.runtime.lastError);
      }
    } else if (existing) {
      existing.remove();
    }
  }

  // Keep Orexa's copy of the YouTube sign-in fresh while you browse (the extension limits how often).
  chrome.runtime.sendMessage({ type: "yt-visit" }, () => void chrome.runtime.lastError);

  window.addEventListener("scroll", () => requestAnimationFrame(place), true);
  window.addEventListener("resize", place);
  setInterval(place, 500);
  window.addEventListener("yt-navigate-finish", ensure);
  setInterval(ensure, 1500); // covers other in-page navigations
  ensure();
})();
