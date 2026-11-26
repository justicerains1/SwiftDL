// SwiftDL 浏览器扩展后台：
// 1. 接管浏览器下载 → 发给本地 SwiftDL；程序未运行时放行浏览器原生下载
// 2. 嗅探网页加载的视频/音频/m3u8，按标签页记录，图标显示数量
import { classify, mergeItem, normalizeKey, parseMaster, suggestName, totalSize } from "./sniff.js";

const DEFAULTS = { enabled: true, minSizeKB: 0, port: 52080, floatBtn: true, token: "" };
const cfg = () => chrome.storage.local.get(DEFAULTS);
// 本地接口应当立刻应答；程序卡住时不能让被暂停的浏览器下载一直等下去
const ADD_TIMEOUT_MS = 5000;

/** 配对信息：优先使用 SwiftDL 释放扩展时写入的 config.json，否则用用户粘贴的令牌 */
export async function pairing() {
  const c = await cfg();
  try {
    const r = await fetch(chrome.runtime.getURL("config.json"));
    if (r.ok) {
      const j = await r.json();
      if (j.token) {
        if (j.token !== c.token || (j.port && j.port !== c.port)) await chrome.storage.local.set({ token: j.token, port: j.port || c.port });
        return { token: j.token, port: j.port || c.port };
      }
    }
  } catch (_) {}
  return { token: c.token, port: c.port };
}

async function send(info) {
  const { port, token } = await pairing();
  let cookies = "";
  try {
    const cs = await chrome.cookies.getAll({ url: info.url });
    cookies = cs.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch (_) {}
  const body = {
    url: info.url,
    filename: info.filename || null,
    referrer: info.referrer || null,
    cookies: cookies || null,
    user_agent: navigator.userAgent,
  };
  const r = await fetch(`http://127.0.0.1:${port}/add`, {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-SwiftDL-Token": token },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(ADD_TIMEOUT_MS),
  });
  if (r.status === 403) throw new Error(chrome.i18n.getMessage("unpairedError") || "unpaired");
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
}

// ───────────── 下载接管 ─────────────
chrome.downloads.onCreated.addListener(async (item) => {
  const { enabled, minSizeKB } = await cfg();
  const url = item.finalUrl || item.url;
  if (!enabled || !/^https?:/i.test(url)) return; // blob:/data: 留给浏览器
  if (minSizeKB > 0 && item.totalBytes > 0 && item.totalBytes < minSizeKB * 1024) return;
  try {
    await chrome.downloads.pause(item.id);
  } catch (_) {}
  try {
    const name = item.filename ? item.filename.split(/[\\/]/).pop() : null;
    await send({ url, filename: name, referrer: item.referrer });
    await chrome.downloads.cancel(item.id);
    await chrome.downloads.erase({ id: item.id });
  } catch (_) {
    try {
      await chrome.downloads.resume(item.id);
    } catch (_) {}
  }
});

// ───────────── 媒体嗅探 ─────────────
const keyOf = (tabId) => `tab:${tabId}`;

async function getList(tabId) {
  const k = keyOf(tabId);
  return (await chrome.storage.session.get(k))[k] || [];
}

async function setList(tabId, list) {
  await chrome.storage.session.set({ [keyOf(tabId)]: list });
  try {
    await chrome.action.setBadgeText({ tabId, text: list.length ? String(list.length) : "" });
    await chrome.action.setBadgeBackgroundColor({ tabId, color: "#2F6FED" });
  } catch (_) {}
}

// 同一标签页的更新串行执行，避免并发覆盖
const queues = new Map();
function serial(tabId, fn) {
  const next = (queues.get(tabId) || Promise.resolve()).then(fn, fn);
  queues.set(
    tabId,
    next.catch(() => {}),
  );
  return next;
}

async function fetchVariants(url) {
  try {
    const r = await fetch(url, { credentials: "include" });
    if (!r.ok) return null;
    return parseMaster(await r.text(), url);
  } catch (_) {
    return null;
  }
}

function record(tabId, item) {
  return serial(tabId, async () => {
    let tab;
    try {
      tab = await chrome.tabs.get(tabId);
    } catch (_) {
      return;
    }
    const list = await getList(tabId);
    if (list.some((x) => x.url === item.url) && item.kind !== "hls") {
      await setList(tabId, mergeItem(list, item));
      return;
    }
    if (item.kind === "hls" && !list.some((x) => x.url === item.url)) item.variants = await fetchVariants(item.url);
    item.title = tab.title || "";
    item.referrer = tab.url || "";
    item.name = suggestName(tab.title, item.url, item.kind);
    item.time = Date.now();
    await setList(tabId, mergeItem(list, item));
  });
}

chrome.webRequest.onHeadersReceived.addListener(
  (d) => {
    if (d.tabId < 0 || d.statusCode >= 400) return;
    const h = {};
    for (const x of d.responseHeaders || []) h[x.name.toLowerCase()] = x.value || "";
    const kind = classify(d.url, h["content-type"], totalSize(h));
    if (!kind) return;
    record(d.tabId, {
      url: normalizeKey(d.url),
      kind,
      mime: (h["content-type"] || "").split(";")[0],
      size: kind === "hls" ? -1 : totalSize(h),
    });
  },
  { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other", "object"] },
  ["responseHeaders"],
);

// 页面跳转或关闭时清空该页记录
chrome.tabs.onUpdated.addListener((tabId, info) => {
  if (info.status === "loading" && info.url) serial(tabId, () => setList(tabId, []));
});
chrome.tabs.onRemoved.addListener((tabId) => {
  chrome.storage.session.remove(keyOf(tabId));
  queues.delete(tabId);
});

// ───────────── 消息：弹窗与网页浮动按钮 ─────────────
chrome.runtime.onMessage.addListener((msg, sender, reply) => {
  if (msg.type === "list") {
    const tabId = msg.tabId ?? sender.tab?.id;
    getList(tabId).then(reply, () => reply([]));
    return true;
  }
  if (msg.type === "pairing") {
    pairing().then(reply, () => reply({ port: 52080, token: "" }));
    return true;
  }
  if (msg.type === "download") {
    send({ url: msg.url, filename: msg.name, referrer: msg.referrer })
      .then(() => reply({ ok: true }))
      .catch((e) => reply({ ok: false, error: String(e) }));
    return true;
  }
  return false;
});

// ───────────── 连接握手与右键菜单 ─────────────
const VERSION = chrome.runtime.getManifest().version;

// 向 SwiftDL 报到（附带扩展版本），程序据此确认扩展已安装并连接，并返回程序版本
async function hello() {
  const { port, token } = await pairing();
  try {
    const r = await fetch(`http://127.0.0.1:${port}/hello`, {
      method: "POST",
      headers: { "X-SwiftDL-Token": token, "X-SwiftDL-Ext-Version": VERSION },
    });
    if (r.ok) await syncVersion((await r.json().catch(() => ({}))).app);
  } catch (_) {}
}

/**
 * 程序升级时会替换扩展文件夹中的文件，但浏览器仍在运行旧代码：版本不一致时重新加载扩展。
 * 每个程序版本最多自动重新加载一次，避免扩展从其他文件夹加载时反复重载。返回是否触发了重新加载。
 */
export async function syncVersion(app) {
  if (!app || app === VERSION) return false;
  const { reloadedFor } = await chrome.storage.local.get({ reloadedFor: "" });
  if (reloadedFor === app) return false;
  await chrome.storage.local.set({ reloadedFor: app });
  chrome.runtime.reload();
  return true;
}

// MV3 后台空闲后会被回收、有事件时再启动：每次启动都报到，程序升级后能及时发现版本变化
hello();

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({ id: "swiftdl-link", title: chrome.i18n.getMessage("menuDownload"), contexts: ["link", "video", "audio", "image"] });
  hello();
});
chrome.runtime.onStartup.addListener(hello);

chrome.contextMenus.onClicked.addListener((info, tab) => {
  const url = info.linkUrl || info.srcUrl;
  if (url && /^https?:/i.test(url)) send({ url, referrer: tab && tab.url }).catch(() => {});
});
