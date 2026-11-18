import { fmtSize } from "./sniff.js";

const DEFAULTS = { enabled: true, minSizeKB: 0, port: 52080, floatBtn: true, token: "" };
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
const TAGS = { video: "视频", audio: "音频", hls: "HLS", dash: "DASH" };

chrome.storage.local.get(DEFAULTS).then(async (c) => {
  $("enabled").checked = c.enabled;
  $("floatBtn").checked = c.floatBtn;
  $("minSizeKB").value = c.minSizeKB;
  $("port").value = c.port;
  await checkConn();
});

async function checkConn() {
  const { port, token } = await chrome.runtime.sendMessage({ type: "pairing" });
  $("token").value = token || "";
  try {
    const r = await fetch(`http://127.0.0.1:${port}/hello`, { method: "POST", headers: { "X-SwiftDL-Token": token || "" } });
    $("st").textContent = r.ok ? "已连接" : r.status === 403 ? "未配对" : "未运行";
    $("st").className = r.ok ? "on" : "";
    $("pair").hidden = r.status !== 403;
  } catch (_) {
    $("st").textContent = "SwiftDL 未运行";
    $("pair").hidden = true;
  }
}

$("savePair").onclick = async () => {
  await chrome.storage.local.set({ token: $("token").value.trim() });
  await checkConn();
};

$("enabled").onchange = (e) => chrome.storage.local.set({ enabled: e.target.checked });
$("floatBtn").onchange = (e) => chrome.storage.local.set({ floatBtn: e.target.checked });
$("minSizeKB").onchange = (e) => chrome.storage.local.set({ minSizeKB: Math.max(0, +e.target.value || 0) });
$("port").onchange = (e) => chrome.storage.local.set({ port: +e.target.value || 52080 });

async function render() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  const list = tab ? await chrome.runtime.sendMessage({ type: "list", tabId: tab.id }) : [];
  const box = $("list");
  if (!list || !list.length) {
    box.innerHTML = '<div class="empty">播放网页中的视频或音频后，可下载的资源会出现在这里</div>';
    return;
  }
  box.innerHTML = list
    .slice()
    .reverse()
    .map((it, i) => {
      const dash = it.kind === "dash";
      let meta = esc(fmtSize(it.size) || (it.kind === "hls" ? "流媒体" : ""));
      if (it.kind === "hls" && it.variants && it.variants.length) {
        meta = `<select data-i="${i}">${it.variants.map((v) => `<option value="${esc(v.url)}">${esc(v.label)}</option>`).join("")}</select>`;
      }
      if (dash) meta = "DASH 暂不支持";
      return `<div class="item"><span class="tag ${it.kind}">${TAGS[it.kind] || it.kind}</span>
        <div class="info"><div class="name" title="${esc(it.url)}">${esc(it.name)}</div><div class="meta">${meta}</div></div>
        <button data-i="${i}" ${dash ? "disabled" : ""}>下载</button></div>`;
    })
    .join("");
  const items = list.slice().reverse();
  box.querySelectorAll("button").forEach((b) =>
    b.addEventListener("click", async () => {
      const i = Number(b.dataset.i);
      const it = items[i];
      const sel = box.querySelector(`select[data-i="${i}"]`);
      const url = sel ? sel.value : it.url;
      b.disabled = true;
      b.textContent = "发送中";
      const r = await chrome.runtime.sendMessage({ type: "download", url, name: it.name, referrer: it.referrer });
      b.textContent = r && r.ok ? "已发送" : "失败";
      if (!(r && r.ok)) $("st").textContent = "SwiftDL 未运行";
    }),
  );
}
render();
