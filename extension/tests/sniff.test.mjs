// 运行：node --test extension/tests
import { test } from "node:test";
import assert from "node:assert/strict";
import { classify, normalizeKey, parseMaster, suggestName, totalSize, mergeItem, fmtSize, MIN_MEDIA } from "../sniff.js";

test("classify: 媒体类型识别", () => {
  assert.equal(classify("https://a.com/v.mp4", "video/mp4", 5e6), "video");
  assert.equal(classify("https://a.com/play", "audio/mpeg; charset=x", 5e6), "audio");
  assert.equal(classify("https://a.com/x/index.m3u8?t=1", "", -1), "hls");
  assert.equal(classify("https://a.com/live", "application/vnd.apple.mpegurl", -1), "hls");
  assert.equal(classify("https://a.com/x.mpd", "", -1), "dash");
  assert.equal(classify("https://a.com/f.mkv", "application/octet-stream", 9e6), "video");
  assert.equal(classify("https://a.com/song.flac", "", 9e6), "audio");
});

test("classify: 过滤分片、小文件和非媒体", () => {
  assert.equal(classify("https://a.com/seg1.ts", "video/mp2t", 9e6), null);
  assert.equal(classify("https://a.com/chunk.m4s", "video/iso.segment", 9e6), null);
  assert.equal(classify("https://a.com/ad.mp4", "video/mp4", MIN_MEDIA - 1), null);
  assert.equal(classify("https://a.com/ad.mp4", "video/mp4", -1), "video", "未知大小不过滤");
  assert.equal(classify("https://a.com/page.html", "text/html", 9e6), null);
  assert.equal(classify("https://a.com/pic.mp4", "image/png", 9e6), null);
  assert.equal(classify("blob:https://a.com/123", "video/mp4", 9e6), null);
  assert.equal(classify("data:video/mp4;base64,AAAA", "video/mp4", 9e6), null);
});

test("normalizeKey: 去掉分段参数", () => {
  assert.equal(
    normalizeKey("https://v.cdn.com/videoplayback?id=1&range=0-1000&rn=3&itag=22#t=5"),
    "https://v.cdn.com/videoplayback?id=1&itag=22",
  );
  assert.equal(normalizeKey("https://a.com/v.mp4?bytestart=100&byteend=200"), "https://a.com/v.mp4");
  assert.equal(normalizeKey("not a url"), "not a url");
});

test("totalSize: Range 响应取总长度", () => {
  assert.equal(totalSize({ "content-range": "bytes 0-1023/52428800", "content-length": "1024" }), 52428800);
  assert.equal(totalSize({ "content-length": "2048" }), 2048);
  assert.equal(totalSize({}), -1);
});

test("parseMaster: 清晰度解析与排序", () => {
  const t = '#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2"\nlow/index.m3u8\n' +
    "#EXT-X-STREAM-INF:BANDWIDTH=4500000,RESOLUTION=1920x1080\nhttps://o.com/hi.m3u8\n";
  const v = parseMaster(t, "https://cdn.com/v/master.m3u8?k=1");
  assert.equal(v.length, 2);
  assert.equal(v[0].url, "https://o.com/hi.m3u8");
  assert.equal(v[0].label, "1080p · 4.5 Mbps");
  assert.equal(v[1].url, "https://cdn.com/v/low/index.m3u8");
  assert.equal(parseMaster("#EXTM3U\n#EXTINF:4,\na.ts\n", "https://a.com/"), null, "媒体列表不是主列表");
  assert.equal(parseMaster("<html>", "https://a.com/"), null);
});

test("suggestName: 用网页标题命名", () => {
  assert.equal(suggestName('精彩视频: 第1集 / "高清"', "https://a.com/x/index.m3u8", "hls"), "精彩视频 第1集 高清.mp4");
  assert.equal(suggestName("", "https://a.com/music/My%20Song.m4a", "audio"), "My Song.m4a");
  assert.equal(suggestName("Title", "https://a.com/play?id=3", "audio"), "Title.mp3");
  assert.equal(suggestName("x".repeat(200), "https://a.com/v.webm", "video").length, 80 + 5);
});

test("mergeItem: 去重、主列表吸收子列表、数量上限", () => {
  let l = [];
  l = mergeItem(l, { url: "https://a.com/v.mp4", kind: "video", size: 100 });
  l = mergeItem(l, { url: "https://a.com/v.mp4", kind: "video", size: 900 });
  assert.equal(l.length, 1);
  assert.equal(l[0].size, 900);
  // 先嗅探到子列表，再嗅探到主列表：子列表被主列表吸收
  l = mergeItem(l, { url: "https://a.com/hi.m3u8", kind: "hls" });
  l = mergeItem(l, { url: "https://a.com/m.m3u8", kind: "hls", variants: [{ url: "https://a.com/hi.m3u8" }] });
  assert.deepEqual(l.map((x) => x.url), ["https://a.com/v.mp4", "https://a.com/m.m3u8"]);
  // 主列表之后再出现的子列表不重复列出
  l = mergeItem(l, { url: "https://a.com/hi.m3u8", kind: "hls" });
  assert.equal(l.length, 2);
  for (let i = 0; i < 60; i++) l = mergeItem(l, { url: `https://a.com/${i}.mp4`, kind: "video" }, 50);
  assert.equal(l.length, 50);
  assert.equal(l.at(-1).url, "https://a.com/59.mp4");
});

test("fmtSize", () => {
  assert.equal(fmtSize(0), "");
  assert.equal(fmtSize(512), "512 B");
  assert.equal(fmtSize(5 * 1024 * 1024), "5.0 MB");
});
