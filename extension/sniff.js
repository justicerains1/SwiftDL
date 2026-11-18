// SwiftDL 嗅探核心：纯函数，供后台脚本使用，并可在 node 中测试。

const VIDEO_EXT = ["mp4", "webm", "mkv", "mov", "flv", "m4v", "avi", "wmv", "3gp", "ogv"];
const AUDIO_EXT = ["mp3", "m4a", "aac", "ogg", "opus", "wav", "flac", "weba", "wma"];
/** 非播放列表的媒体小于此值视为广告/图标/预览，忽略 */
export const MIN_MEDIA = 100 * 1024;
/** 这些查询参数只表示“同一文件的某一段”，去重与下载时去掉 */
const RANGE_PARAMS = ["range", "rn", "rbuf", "bytestart", "byteend"];

export function extOf(url) {
  try {
    const m = new URL(url).pathname.toLowerCase().match(/\.([a-z0-9]{2,5})$/);
    return m ? m[1] : "";
  } catch (_) {
    return "";
  }
}

/** 判断一个响应是否是可下载的媒体：返回 video / audio / hls / dash / null */
export function classify(url, contentType = "", size = -1) {
  if (!/^https?:/i.test(url)) return null;
  const ct = (contentType || "").toLowerCase().split(";")[0].trim();
  const ext = extOf(url);
  if (ct.includes("mpegurl") || ext === "m3u8") return "hls";
  if (ct === "application/dash+xml" || ext === "mpd") return "dash";
  // HLS / DASH 的分片不单独列出
  if (ct === "video/mp2t" || ct === "video/iso.segment" || ext === "ts" || ext === "m4s") return null;
  const generic = ct === "" || ct === "application/octet-stream" || ct.startsWith("binary/");
  let kind = null;
  if (ct.startsWith("video/")) kind = "video";
  else if (ct.startsWith("audio/")) kind = "audio";
  else if (generic && VIDEO_EXT.includes(ext)) kind = "video";
  else if (generic && AUDIO_EXT.includes(ext)) kind = "audio";
  if (!kind) return null;
  if (size >= 0 && size < MIN_MEDIA) return null;
  return kind;
}

/** 去掉分段参数与锚点，用于去重（同一视频的多次 Range 请求只算一条） */
export function normalizeKey(url) {
  try {
    const u = new URL(url);
    for (const p of RANGE_PARAMS) u.searchParams.delete(p);
    u.hash = "";
    return u.toString();
  } catch (_) {
    return url;
  }
}

/** 从响应头得到文件总大小（Range 响应取 Content-Range 的总长度） */
export function totalSize(headers) {
  const cr = headers["content-range"];
  const m = cr && cr.match(/\/(\d+)\s*$/);
  if (m) return Number(m[1]);
  const cl = headers["content-length"];
  return cl ? Number(cl) : -1;
}

function attrs(s) {
  const out = {};
  const re = /([A-Z0-9-]+)=("[^"]*"|[^,]*)/gi;
  let m;
  while ((m = re.exec(s))) out[m[1].toUpperCase()] = m[2].replace(/^"|"$/g, "");
  return out;
}

export function qualityLabel(resolution, bandwidth) {
  const h = resolution && resolution.includes("x") ? `${resolution.split("x")[1]}p` : "";
  const bw = bandwidth ? `${(bandwidth / 1e6).toFixed(1)} Mbps` : "";
  return [h, bw].filter(Boolean).join(" · ") || "默认清晰度";
}

/** 解析主播放列表，返回按码率从高到低的清晰度；不是主列表返回 null */
export function parseMaster(text, base) {
  if (!text || !text.trimStart().startsWith("#EXTM3U") || !text.includes("#EXT-X-STREAM-INF")) return null;
  const lines = text.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  const out = [];
  let pending = null;
  for (const l of lines) {
    if (l.startsWith("#EXT-X-STREAM-INF:")) pending = attrs(l.slice(18));
    else if (!l.startsWith("#") && pending) {
      let url;
      try {
        url = new URL(l, base).toString();
      } catch (_) {
        pending = null;
        continue;
      }
      const bandwidth = Number(pending.BANDWIDTH || 0);
      const resolution = pending.RESOLUTION || "";
      out.push({ url, bandwidth, resolution, label: qualityLabel(resolution, bandwidth) });
      pending = null;
    }
  }
  out.sort((a, b) => b.bandwidth - a.bandwidth);
  return out.length ? out : null;
}

/** 用网页标题生成文件名 */
export function suggestName(title, url, kind) {
  const ext = kind === "hls" ? "mp4" : extOf(url) || (kind === "audio" ? "mp3" : "mp4");
  let t = (title || "").replace(/[<>:"/\\|?*\u0000-\u001f]/g, " ").replace(/\s+/g, " ").trim();
  if (t.length > 80) t = t.slice(0, 80).trim();
  if (!t) {
    try {
      const last = decodeURIComponent(new URL(url).pathname.split("/").filter(Boolean).pop() || "");
      t = last.replace(/\.[a-z0-9]{2,5}$/i, "") || "video";
    } catch (_) {
      t = "video";
    }
  }
  return `${t}.${ext}`;
}

export function fmtSize(n) {
  if (!(n > 0)) return "";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return i === 0 ? `${n} B` : `${n.toFixed(1)} ${u[i]}`;
}

/**
 * 把新嗅探到的资源合并进列表：同一地址只保留一条；主列表的各清晰度子列表不重复出现。
 * 返回新列表（最多 max 条，超出丢弃最早的）。
 */
export function mergeItem(list, item, max = 50) {
  const exist = list.find((x) => x.url === item.url);
  if (exist) {
    if (item.size > (exist.size || -1)) exist.size = item.size;
    if (item.variants && !exist.variants) exist.variants = item.variants;
    return list;
  }
  const isChildOfMaster = list.some((x) => x.variants && x.variants.some((v) => v.url === item.url));
  if (isChildOfMaster) return list;
  let next = list;
  if (item.variants) next = next.filter((x) => !item.variants.some((v) => v.url === x.url));
  next = [...next, item];
  return next.length > max ? next.slice(next.length - max) : next;
}
