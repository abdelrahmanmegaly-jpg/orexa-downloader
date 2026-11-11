const $ = (id) => document.getElementById(id);
const DEFAULTS = { enabled: true, minSizeMB: 0, onlyTypes: "", skipSites: "", showDialog: true, sniff: true, catchImages: false };

async function showStatus() {
  const el = $("status");
  const reply = await chrome.runtime.sendMessage({ type: "ping" });
  if (reply && reply.ok) {
    el.textContent = reply.running ? "Connected, Orexa is running" : "Connected, Orexa will start when needed";
    el.className = "ok";
  } else {
    el.textContent = "Orexa Downloader is not installed or has not been opened yet";
    el.className = "bad";
  }
}

async function showRecent() {
  const { recent = [] } = await chrome.storage.local.get("recent");
  const list = $("recent");
  list.textContent = "";
  if (!recent.length) {
    const li = document.createElement("li");
    li.className = "muted";
    li.textContent = "Nothing yet";
    list.appendChild(li);
    return;
  }
  for (const r of recent) {
    const li = document.createElement("li");
    li.textContent = r.filename || r.url;
    li.title = r.url;
    list.appendChild(li);
  }
}

function fmtSize(n) {
  if (!n) return "";
  const u = ["B", "KB", "MB", "GB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${u[i]}`;
}

async function showFound() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  const box = $("foundBox");
  const list = $("found");
  list.textContent = "";
  if (!tab) {
    box.hidden = true;
    return;
  }
  const reply = await chrome.runtime.sendMessage({ type: "media-list", tabId: tab.id });
  const items = (reply && reply.items) || [];
  box.hidden = !items.length;
  for (const it of items) {
    const li = document.createElement("li");
    li.className = "found";
    const name = document.createElement("span");
    name.className = "fname";
    name.dir = "auto";
    name.textContent = it.name;
    name.title = it.name;
    const size = document.createElement("span");
    size.className = "muted";
    size.textContent = fmtSize(it.size);
    const go = document.createElement("button");
    go.type = "button";
    go.className = "mini";
    go.textContent = "Download";
    go.addEventListener("click", async () => {
      const r = await chrome.runtime.sendMessage({ type: "media-download", tabId: tab.id, key: it.key });
      if (r && r.ok) window.close();
      else {
        $("status").textContent = "Could not reach Orexa";
        $("status").className = "bad";
      }
    });
    li.append(name, size, go);
    list.appendChild(li);
  }
}

async function init() {
  const cfg = { ...DEFAULTS, ...(await chrome.storage.sync.get(DEFAULTS)) };
  $("enabled").checked = cfg.enabled;
  $("sniff").checked = cfg.sniff;
  $("catchImages").checked = cfg.catchImages;
  $("minSizeMB").value = cfg.minSizeMB;
  $("onlyTypes").value = cfg.onlyTypes;
  $("skipSites").value = cfg.skipSites;

  $("sniff").addEventListener("change", (e) => chrome.storage.sync.set({ sniff: e.target.checked }));
  $("catchImages").addEventListener("change", (e) => chrome.storage.sync.set({ catchImages: e.target.checked }));
  $("enabled").addEventListener("change", (e) => chrome.storage.sync.set({ enabled: e.target.checked }));
  $("minSizeMB").addEventListener("change", (e) => chrome.storage.sync.set({ minSizeMB: Math.max(0, Number(e.target.value) || 0) }));
  $("onlyTypes").addEventListener("change", (e) => chrome.storage.sync.set({ onlyTypes: e.target.value }));
  $("skipSites").addEventListener("change", (e) => chrome.storage.sync.set({ skipSites: e.target.value }));

  $("send").addEventListener("click", async () => {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    if (!tab || !/^https?:/i.test(tab.url || "")) {
      $("status").textContent = "This page cannot be sent";
      $("status").className = "bad";
      return;
    }
    const reply = await chrome.runtime.sendMessage({ type: "send-url", url: tab.url });
    $("status").textContent = reply && reply.ok ? "Sent to Orexa" : "Could not reach Orexa";
    $("status").className = reply && reply.ok ? "ok" : "bad";
    showRecent();
  });
  $("open").addEventListener("click", () => chrome.runtime.sendMessage({ type: "open" }));

  showStatus();
  showRecent();
  showFound();
}
init();
