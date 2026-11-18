const DEFAULTS = { enabled: true, minSizeKB: 0, port: 52080 };
const $ = (id) => document.getElementById(id);

chrome.storage.local.get(DEFAULTS).then(async (c) => {
  $("enabled").checked = c.enabled;
  $("minSizeKB").value = c.minSizeKB;
  $("port").value = c.port;
  try {
    const r = await fetch(`http://127.0.0.1:${c.port}/hello`, { method: "POST" });
    $("st").textContent = r.ok ? "● 已连接" : "○ 未运行";
  } catch (_) {
    $("st").textContent = "○ 未运行";
  }
});

$("enabled").onchange = (e) => chrome.storage.local.set({ enabled: e.target.checked });
$("minSizeKB").onchange = (e) => chrome.storage.local.set({ minSizeKB: Math.max(0, +e.target.value || 0) });
$("port").onchange = (e) => chrome.storage.local.set({ port: +e.target.value || 52080 });
