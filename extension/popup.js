import { fmtSize } from "./sniff.js";

const DEFAULTS = { enabled: true, minSizeKB: 0, port: 52080, floatBtn: true, token: "" };
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
const t = (k) => chrome.i18n.getMessage(k) || k;
const TAGS = { video: t("tagVideo"), audio: t("tagAudio"), hls: "HLS", dash: "DASH" };
document.querySelectorAll("[data-i18n]").forEach((el) => (el.textContent = t(el.dataset.i18n)));
document.querySelectorAll("[data-i18n-ph]").forEach((el) => (el.placeholder = t(el.dataset.i18nPh)));

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
    const ver = chrome.runtime.getManifest().version;
    const r = await fetch(`http://127.0.0.1:${port}/hello`, {
      method: "POST",
      headers: { "X-SwiftDL-Token": token || "", "X-SwiftDL-Ext-Version": ver },
    });
    $("st").textContent = t(r.ok ? "statusConnected" : r.status === 403 ? "statusUnpaired" : "statusNotRunning");
    $("st").className = r.ok ? "on" : "";
    $("pair").hidden = r.status !== 403;
    // 自动重新加载后仍不一致：多半是从其他文件夹加载的扩展，提示用户处理
    const app = r.ok ? (await r.json().catch(() => ({}))).app : "";
    $("ver").hidden = !app || app === ver;
    $("ver").textContent = `${t("versionMismatch")} (${ver} / SwiftDL ${app})`;
  } catch (_) {
    $("st").textContent = t("statusAppNotRunning");
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
    box.innerHTML = `<div class="empty">${esc(t("empty"))}</div>`;
    return;
  }
  box.innerHTML = list
    .slice()
    .reverse()
    .map((it, i) => {
      const dash = it.kind === "dash";
      let meta = esc(fmtSize(it.size) || (it.kind === "hls" ? t("stream") : ""));
      if (it.kind === "hls" && it.variants && it.variants.length) {
        meta = `<select data-i="${i}">${it.variants.map((v) => `<option value="${esc(v.url)}">${esc(v.label)}</option>`).join("")}</select>`;
      }
      if (dash) meta = esc(t("dashUnsupported"));
      return `<div class="item"><span class="tag ${it.kind}">${TAGS[it.kind] || it.kind}</span>
        <div class="info"><div class="name" title="${esc(it.url)}">${esc(it.name)}</div><div class="meta">${meta}</div></div>
        <button data-i="${i}" ${dash ? "disabled" : ""}>${esc(t("btnDownload"))}</button></div>`;
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
      b.textContent = t("btnSending");
      const r = await chrome.runtime.sendMessage({ type: "download", url, name: it.name, referrer: it.referrer });
      b.textContent = t(r && r.ok ? "btnSent" : "btnFailed");
      if (!(r && r.ok)) $("st").textContent = t("statusAppNotRunning");
    }),
  );
}
render();
