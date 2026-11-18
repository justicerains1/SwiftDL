// 在 node 中用模拟的 chrome API 运行 background.js，验证嗅探 → 列表 → 发送到 SwiftDL 的完整流程
import { test } from "node:test";
import assert from "node:assert/strict";

const listeners = {};
const on = (name) => ({ addListener: (f) => (listeners[name] = f) });
const session = {};
const badges = {};
const posted = [];
const tabs = { 7: { id: 7, title: "测试视频页", url: "https://site.com/watch/1" } };

globalThis.chrome = {
  storage: {
    local: { get: async (d) => ({ ...d }) },
    session: {
      get: async (k) => (k in session ? { [k]: session[k] } : {}),
      set: async (o) => Object.assign(session, o),
      remove: async (k) => delete session[k],
    },
  },
  action: { setBadgeText: async ({ tabId, text }) => (badges[tabId] = text), setBadgeBackgroundColor: async () => {} },
  tabs: { get: async (id) => tabs[id] ?? Promise.reject(new Error("no tab")), onUpdated: on("tabUpdated"), onRemoved: on("tabRemoved") },
  webRequest: { onHeadersReceived: on("headers") },
  runtime: { onMessage: on("message"), onInstalled: on("installed"), onStartup: on("startup") },
  downloads: { onCreated: on("dlCreated") },
  contextMenus: { create() {}, onClicked: on("menu") },
  cookies: { getAll: async () => [{ name: "sid", value: "abc" }] },
};
globalThis.fetch = async (url, opt = {}) => {
  if (url === "https://cdn.com/v/master.m3u8") {
    return new Response("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720\nhigh.m3u8\n");
  }
  if (url.startsWith("http://127.0.0.1:52080/add")) {
    posted.push(JSON.parse(opt.body));
    return new Response("", { status: 200 });
  }
  return new Response("", { status: 404 });
};

await import("../background.js");
const hdr = (o) => Object.entries(o).map(([name, value]) => ({ name, value }));
const fire = (url, headers, tabId = 7, statusCode = 200) => listeners.headers({ url, tabId, statusCode, responseHeaders: hdr(headers) });
const settle = () => new Promise((r) => setTimeout(r, 30));
const msg = (m) => new Promise((r) => listeners.message(m, {}, r));

test("嗅探 → 列表 → 角标 → 发送", async () => {
  fire("https://cdn.com/v.mp4?range=0-1000", { "content-type": "video/mp4", "content-range": "bytes 0-1000/50000000" });
  fire("https://cdn.com/v.mp4?range=1001-2000", { "content-type": "video/mp4", "content-range": "bytes 1001-2000/50000000" });
  fire("https://cdn.com/ad.mp4", { "content-type": "video/mp4", "content-length": "2000" }); // 太小
  fire("https://cdn.com/seg1.ts", { "content-type": "video/mp2t", "content-length": "900000" }); // 分片
  fire("https://cdn.com/v/high.m3u8", { "content-type": "application/vnd.apple.mpegurl" }); // 子列表先到
  fire("https://cdn.com/v/master.m3u8", { "content-type": "application/vnd.apple.mpegurl" });
  fire("https://cdn.com/v/low.m3u8", { "content-type": "application/vnd.apple.mpegurl" }); // 子列表后到
  fire("https://cdn.com/x.mp4", { "content-type": "video/mp4" }, -1); // 非标签页请求
  fire("https://cdn.com/err.mp4", { "content-type": "video/mp4" }, 7, 404);
  await settle();

  const list = await msg({ type: "list", tabId: 7 });
  assert.deepEqual(list.map((x) => [x.kind, x.url]), [
    ["video", "https://cdn.com/v.mp4"],
    ["hls", "https://cdn.com/v/master.m3u8"],
  ]);
  assert.equal(list[0].size, 50000000);
  assert.equal(list[0].name, "测试视频页.mp4");
  assert.equal(list[0].referrer, "https://site.com/watch/1");
  assert.deepEqual(list[1].variants.map((v) => v.label), ["720p · 3.0 Mbps", "360p · 0.9 Mbps"]);
  assert.equal(badges[7], "2");

  const r = await msg({ type: "download", url: list[1].variants[0].url, name: list[1].name, referrer: list[1].referrer });
  assert.deepEqual(r, { ok: true });
  assert.equal(posted.at(-1).url, "https://cdn.com/v/high.m3u8");
  assert.equal(posted.at(-1).cookies, "sid=abc");
  assert.equal(posted.at(-1).referrer, "https://site.com/watch/1");
  assert.equal(posted.at(-1).filename, "测试视频页.mp4");
});

test("页面跳转清空，关闭标签删除记录", async () => {
  listeners.tabUpdated(7, { status: "loading", url: "https://site.com/other" });
  await settle();
  assert.deepEqual(await msg({ type: "list", tabId: 7 }), []);
  assert.equal(badges[7], "");
  fire("https://cdn.com/a.mp3", { "content-type": "audio/mpeg", "content-length": "4000000" });
  await settle();
  assert.equal((await msg({ type: "list", tabId: 7 })).length, 1);
  listeners.tabRemoved(7);
  assert.equal("tab:7" in session, false);
});

test("SwiftDL 未运行时下载返回失败", async () => {
  const real = globalThis.fetch;
  globalThis.fetch = async () => {
    throw new TypeError("Failed to fetch");
  };
  const r = await msg({ type: "download", url: "https://cdn.com/a.mp4", name: "a.mp4" });
  globalThis.fetch = real;
  assert.equal(r.ok, false);
});
