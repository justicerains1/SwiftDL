// SwiftDL 浏览器接管：拦截下载 → 发给本地 SwiftDL；程序未运行时放行浏览器原生下载。
const DEFAULTS = { enabled: true, minSizeKB: 0, port: 52080 };
const cfg = () => chrome.storage.local.get(DEFAULTS);

async function send(info) {
  const { port } = await cfg();
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
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
}

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

// 向 SwiftDL 报到，程序据此确认扩展已安装并连接
async function hello() {
  const { port } = await cfg();
  try {
    await fetch(`http://127.0.0.1:${port}/hello`, { method: "POST" });
  } catch (_) {}
}

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({ id: "swiftdl-link", title: "使用 SwiftDL 下载", contexts: ["link", "video", "audio", "image"] });
  hello();
});
chrome.runtime.onStartup.addListener(hello);

chrome.contextMenus.onClicked.addListener((info, tab) => {
  const url = info.linkUrl || info.srcUrl;
  if (url) send({ url, referrer: tab && tab.url }).catch(() => {});
});
