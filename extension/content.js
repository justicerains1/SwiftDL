// SwiftDL 视频浮动下载按钮：鼠标悬停在网页视频上时出现，点击选择清晰度后交给 SwiftDL 下载。
(() => {
  if (window.__swiftdlFloat) return;
  window.__swiftdlFloat = true;

  let enabled = true;
  let dismissed = false; // 本页点了关闭
  chrome.storage.local.get({ floatBtn: true }).then((c) => (enabled = c.floatBtn));
  chrome.storage.onChanged.addListener((ch) => {
    if (ch.floatBtn) enabled = ch.floatBtn.newValue;
    if (!enabled) hide();
  });

  const MIN_W = 200;
  const MIN_H = 120;
  const ICON_DL =
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M8 2.5v7.5M4.5 6.8 8 10.3l3.5-3.5M2.8 13.5h10.4"/></svg>';
  const ICON_X =
    '<svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M4 4l8 8M12 4l-8 8"/></svg>';

  const host = document.createElement("div");
  host.style.cssText = "all:initial;position:fixed;z-index:2147483647;top:0;left:0;display:none;";
  const root = host.attachShadow({ mode: "closed" });
  root.innerHTML = `
    <style>
      * { box-sizing: border-box; font: 13px/1.4 "Segoe UI", "Microsoft YaHei", system-ui, sans-serif; }
      .bar { display: flex; align-items: center; gap: 2px; background: #2f6fed; color: #fff; border-radius: 8px;
             box-shadow: 0 4px 14px rgba(0,0,0,.28); user-select: none; }
      .btn { display: flex; align-items: center; gap: 6px; padding: 6px 10px; cursor: pointer; border-radius: 8px 0 0 8px; }
      .btn:hover, .x:hover { background: rgba(255,255,255,.16); }
      .x { display: flex; align-items: center; padding: 8px 7px; cursor: pointer; border-radius: 0 8px 8px 0; opacity: .85; }
      .menu { margin-top: 6px; min-width: 240px; max-width: 380px; max-height: 320px; overflow: auto; background: #fff;
              color: #1f2937; border-radius: 10px; box-shadow: 0 10px 30px rgba(0,0,0,.25); padding: 6px; display: none; }
      .item { display: flex; align-items: center; gap: 8px; padding: 7px 8px; border-radius: 6px; cursor: pointer; }
      .item:hover { background: #eef0f3; }
      .tag { font-size: 11px; font-weight: 600; padding: 1px 6px; border-radius: 4px; background: #e8efff; color: #2f6fed; flex: none; }
      .tag.audio { background: #fde7f3; color: #db2777; }
      .name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
      .meta { color: #6b7280; font-size: 12px; flex: none; }
      .tip { padding: 8px; color: #6b7280; }
      .ok { color: #16a34a; } .err { color: #dc2626; }
    </style>
    <div class="bar"><div class="btn" title="使用 SwiftDL 下载">${ICON_DL}<span>下载此视频</span></div><div class="x" title="本页不再显示">${ICON_X}</div></div>
    <div class="menu"></div>`;
  const bar = root.querySelector(".bar");
  const btn = root.querySelector(".btn");
  const menu = root.querySelector(".menu");
  root.querySelector(".x").addEventListener("click", (e) => {
    e.stopPropagation();
    dismissed = true;
    hide();
  });

  let current = null;
  let hideTimer = 0;
  let overUi = false;

  function mount() {
    const parent = document.fullscreenElement || document.documentElement;
    if (host.parentNode !== parent) parent.appendChild(host);
  }
  function hide() {
    host.style.display = "none";
    menu.style.display = "none";
    current = null;
  }
  function place() {
    if (!current) return;
    const r = current.getBoundingClientRect();
    if (r.width < MIN_W || r.height < MIN_H || r.bottom < 0 || r.top > innerHeight) return hide();
    const w = bar.offsetWidth || 130;
    host.style.left = `${Math.max(4, Math.min(r.right, innerWidth) - w - 10)}px`;
    host.style.top = `${Math.max(4, r.top) + 10}px`;
  }
  function show(v) {
    if (!enabled || dismissed) return;
    clearTimeout(hideTimer);
    if (current !== v) menu.style.display = "none";
    current = v;
    mount();
    host.style.display = "block";
    place();
  }
  function scheduleHide() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      if (!overUi && menu.style.display !== "block") hide();
    }, 1500);
  }

  // 播放器常在视频上覆盖控件层，因此按坐标判断指针是否在某个视频区域内
  let lastMove = 0;
  document.addEventListener(
    "mousemove",
    (e) => {
      const now = Date.now();
      if (now - lastMove < 120 || !enabled || dismissed) return;
      lastMove = now;
      const vids = document.querySelectorAll("video");
      for (const v of vids) {
        const r = v.getBoundingClientRect();
        if (r.width >= MIN_W && r.height >= MIN_H && e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom) {
          return show(v);
        }
      }
      if (current) scheduleHide();
    },
    { passive: true, capture: true },
  );
  host.addEventListener("mouseenter", () => {
    overUi = true;
    clearTimeout(hideTimer);
  });
  host.addEventListener("mouseleave", () => {
    overUi = false;
    scheduleHide();
  });
  addEventListener("scroll", place, { passive: true, capture: true });
  addEventListener("resize", place, { passive: true });
  document.addEventListener("fullscreenchange", () => current && (mount(), place()));
  document.addEventListener("click", (e) => {
    if (!e.composedPath().includes(host)) menu.style.display = "none";
  });

  function esc(s) {
    return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
  }
  function fmtSize(n) {
    if (!(n > 0)) return "";
    const u = ["B", "KB", "MB", "GB"];
    let i = 0;
    while (n >= 1024 && i < u.length - 1) (n /= 1024), i++;
    return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
  }

  btn.addEventListener("click", async (e) => {
    e.stopPropagation();
    if (menu.style.display === "block") return (menu.style.display = "none");
    menu.innerHTML = '<div class="tip">正在查找可下载的视频</div>';
    menu.style.display = "block";
    let list = [];
    try {
      list = await chrome.runtime.sendMessage({ type: "list" });
    } catch (_) {}
    const opts = [];
    for (const it of list || []) {
      if (it.kind === "hls") {
        if (it.variants && it.variants.length) {
          for (const v of it.variants) opts.push({ tag: "HLS", name: it.name, meta: v.label, url: v.url, file: it.name });
        } else opts.push({ tag: "HLS", name: it.name, meta: "流媒体", url: it.url, file: it.name });
      } else if (it.kind === "video" || it.kind === "audio") {
        opts.push({ tag: it.kind === "audio" ? "音频" : "视频", cls: it.kind, name: it.name, meta: fmtSize(it.size), url: it.url, file: it.name });
      }
    }
    const src = current && (current.currentSrc || current.src);
    if (src && /^https?:/i.test(src) && !opts.some((o) => o.url === src)) {
      opts.unshift({ tag: "视频", name: document.title || "video", meta: "当前播放", url: src, file: `${(document.title || "video").replace(/[<>:"/\\|?*]/g, " ").trim()}.mp4` });
    }
    if (!opts.length) {
      menu.innerHTML = '<div class="tip">未检测到可下载的视频，请先播放视频。<br>受 DRM 保护的视频无法下载。</div>';
      return;
    }
    menu.innerHTML = opts
      .map(
        (o, i) =>
          `<div class="item" data-i="${i}"><span class="tag ${o.cls || ""}">${esc(o.tag)}</span><span class="name" title="${esc(o.name)}">${esc(o.name)}</span><span class="meta">${esc(o.meta || "")}</span></div>`,
      )
      .join("");
    menu.querySelectorAll(".item").forEach((el) =>
      el.addEventListener("click", async (ev) => {
        ev.stopPropagation();
        const o = opts[Number(el.dataset.i)];
        let r;
        try {
          r = await chrome.runtime.sendMessage({ type: "download", url: o.url, name: o.file, referrer: location.href });
        } catch (_) {}
        menu.innerHTML = r && r.ok ? '<div class="tip ok">已发送到 SwiftDL</div>' : '<div class="tip err">发送失败：请确认 SwiftDL 正在运行</div>';
        setTimeout(() => (menu.style.display = "none"), 1600);
      }),
    );
  });
})();
