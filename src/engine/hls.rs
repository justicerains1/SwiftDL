//! HLS（m3u8）流媒体下载：播放列表解析、清晰度选择、分片并发下载有序写入、AES-128 解密、续传、ffmpeg 合并为 MP4。
//! 不处理 DRM（SAMPLE-AES、Widevine 等），遇到时明确提示。
use super::probe::{err_str, with_headers};
use super::sys::write_at;
use super::{AddReq, CANCELLED, Manager, Task, backoff, now};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::*;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_RETRY: u32 = 6;
/// 播放列表最大 8 MiB
const MAX_PLAYLIST: usize = 8 << 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub resolution: Option<String>,
}

impl Variant {
    /// 如 “1080p  4.5 Mbps”
    pub fn label(&self) -> String {
        let q = self
            .resolution
            .as_deref()
            .and_then(|r| r.split_once('x').map(|(_, h)| format!("{h}p")))
            .unwrap_or_else(|| tr!("未知清晰度").into());
        if self.bandwidth > 0 { format!("{q}  {:.1} Mbps", self.bandwidth as f64 / 1e6) } else { q }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    pub uri: String,
    pub iv: Option<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub url: String,
    pub duration: f64,
    /// (偏移, 长度)
    pub range: Option<(u64, u64)>,
    pub key: Option<Key>,
    pub seq: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Media {
    pub segments: Vec<Segment>,
    /// fMP4 初始化段 (URL, 字节范围)
    pub map: Option<(String, Option<(u64, u64)>)>,
    pub endlist: bool,
    pub drm: bool,
}

impl Media {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }
    pub fn encrypted(&self) -> bool {
        self.segments.iter().any(|s| s.key.is_some())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Playlist {
    Master(Vec<Variant>),
    Media(Media),
}

/// 探测结果（新建对话框与任务共用）
#[derive(Clone, Debug)]
pub struct HlsInfo {
    /// 主列表中的清晰度（按码率从高到低），直接给媒体列表时为空
    pub variants: Vec<Variant>,
    /// 将要下载的媒体列表 URL（默认最高清晰度）
    pub media_url: String,
    pub segments: usize,
    pub duration: f64,
    pub encrypted: bool,
    pub fmp4: bool,
    pub drm: bool,
    pub live: bool,
}

/// 持久化的下载进度
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HlsState {
    pub media_url: String,
    pub total: u32,
    /// 已按顺序写入文件的分片数
    pub done: u32,
    /// 已写入字节数（含初始化段）
    pub bytes: u64,
    pub fmp4: bool,
    pub encrypted: bool,
    pub duration: f64,
    /// 已写入初始化段
    pub map_done: bool,
    /// 结果说明（如“未找到 ffmpeg，已保存为 .ts”）
    pub note: Option<String>,
}

// ───────────── 解析 ─────────────

/// 解析属性列表：KEY=VALUE,KEY="VALUE,含逗号"
fn attrs(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_ascii_uppercase();
        rest = &rest[eq + 1..];
        let val;
        if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            val = r[..end].to_string();
            rest = r.get(end + 1..).unwrap_or("");
            rest = rest.trim_start_matches(',');
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            val = rest[..end].trim().to_string();
            rest = rest.get(end + 1..).unwrap_or("");
        }
        out.insert(key, val);
        rest = rest.trim_start();
    }
    out
}

fn join(base: &str, uri: &str) -> Result<String, String> {
    url::Url::parse(base)
        .and_then(|b| b.join(uri.trim()))
        .map(|u| u.to_string())
        .map_err(|e| trf!("无效的分片地址 {}: {}", uri, e))
}

/// "len@offset" 或 "len"（偏移接续上一段）
fn byterange(s: &str, prev_end: u64) -> Option<(u64, u64)> {
    let s = s.trim().trim_matches('"');
    let (len, off) = match s.split_once('@') {
        Some((l, o)) => (l.parse().ok()?, o.parse().ok()?),
        None => (s.parse().ok()?, prev_end),
    };
    Some((off, len))
}

fn parse_iv(s: &str) -> Option<[u8; 16]> {
    let h = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    let v = u128::from_str_radix(h, 16).ok()?;
    Some(v.to_be_bytes())
}

pub fn parse(text: &str, base: &str) -> Result<Playlist, String> {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return Err(tr!("不是有效的 m3u8 播放列表").into());
    }
    let all: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();

    // 主播放列表
    if all.iter().any(|l| l.starts_with("#EXT-X-STREAM-INF")) {
        let mut vs = Vec::new();
        let mut pending: Option<HashMap<String, String>> = None;
        for l in &all {
            if let Some(a) = l.strip_prefix("#EXT-X-STREAM-INF:") {
                pending = Some(attrs(a));
            } else if !l.starts_with('#')
                && let Some(a) = pending.take()
            {
                vs.push(Variant {
                    url: join(base, l)?,
                    bandwidth: a.get("BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                    resolution: a.get("RESOLUTION").cloned(),
                });
            }
        }
        if vs.is_empty() {
            return Err(tr!("主播放列表中没有可用的清晰度").into());
        }
        vs.sort_by_key(|v| std::cmp::Reverse(v.bandwidth));
        return Ok(Playlist::Master(vs));
    }

    // 媒体播放列表
    let mut m = Media::default();
    let mut seq: u64 = 0;
    let mut dur = 0.0;
    let mut range: Option<(u64, u64)> = None;
    let mut key: Option<Key> = None;
    let mut last_end: HashMap<String, u64> = HashMap::new();
    for l in &all {
        if let Some(v) = l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("#EXTINF:") {
            dur = v.split(',').next().and_then(|d| d.trim().parse().ok()).unwrap_or(0.0);
        } else if let Some(v) = l.strip_prefix("#EXT-X-BYTERANGE:") {
            // u64::MAX 表示未给偏移，稍后按“接续上一段”解析
            range = byterange(v, u64::MAX);
        } else if let Some(v) = l.strip_prefix("#EXT-X-KEY:") {
            let a = attrs(v);
            let method = a.get("METHOD").map(|s| s.to_ascii_uppercase()).unwrap_or_default();
            let format_ok = a.get("KEYFORMAT").is_none_or(|f| f.eq_ignore_ascii_case("identity"));
            match method.as_str() {
                "NONE" => key = None,
                "AES-128" if format_ok => {
                    let uri = a.get("URI").ok_or(tr!("加密密钥缺少 URI"))?;
                    key = Some(Key { uri: join(base, uri)?, iv: a.get("IV").and_then(|s| parse_iv(s)) });
                }
                _ => m.drm = true,
            }
        } else if let Some(v) = l.strip_prefix("#EXT-X-MAP:") {
            let a = attrs(v);
            let uri = a.get("URI").ok_or(tr!("初始化段缺少 URI"))?;
            m.map = Some((join(base, uri)?, a.get("BYTERANGE").and_then(|r| byterange(r, 0))));
        } else if *l == "#EXT-X-ENDLIST" {
            m.endlist = true;
        } else if !l.starts_with('#') {
            let url = join(base, l)?;
            // BYTERANGE 未给偏移时接续同一资源上一段的结尾
            let r = range.take().map(|(off, len)| {
                let off = if off == u64::MAX { *last_end.get(&url).unwrap_or(&0) } else { off };
                (off, len)
            });
            if let Some((off, len)) = r {
                last_end.insert(url.clone(), off + len);
            }
            m.segments.push(Segment { url, duration: dur, range: r, key: key.clone(), seq });
            seq += 1;
            dur = 0.0;
        }
    }
    Ok(Playlist::Media(m))
}

/// AES-128-CBC 解密（PKCS7 去填充）
pub fn decrypt(key: &[u8; 16], iv: &[u8; 16], data: &mut Vec<u8>) -> Result<(), String> {
    use aes::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
    let n = cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded::<Pkcs7>(data)
        .map_err(|_| tr!("分片解密失败（密钥错误或数据损坏）").to_string())?
        .len();
    data.truncate(n);
    Ok(())
}

pub fn segment_iv(seg: &Segment) -> [u8; 16] {
    seg.key.as_ref().and_then(|k| k.iv).unwrap_or_else(|| (seg.seq as u128).to_be_bytes())
}

// ───────────── 网络 ─────────────

pub fn looks_like_hls(url: &str, content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("mpegurl") {
        return true;
    }
    url::Url::parse(url).is_ok_and(|u| u.path().to_ascii_lowercase().ends_with(".m3u8"))
}

async fn get_text(client: &reqwest::Client, req: &AddReq, url: &str) -> Result<String, String> {
    let resp = with_headers(client.get(url), req).send().await.map_err(err_str)?;
    let st = resp.status().as_u16();
    if !(200..300).contains(&st) {
        return Err(trf!("获取播放列表失败 (HTTP {})", st));
    }
    let b = resp.bytes().await.map_err(err_str)?;
    if b.len() > MAX_PLAYLIST {
        return Err(tr!("播放列表过大").into());
    }
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// 获取并解析媒体列表；给的是主列表时选择最高码率
pub async fn load(client: &reqwest::Client, req: &AddReq, url: &str) -> Result<(HlsInfo, Media), String> {
    let text = get_text(client, req, url).await?;
    let (variants, media_url, media) = match parse(&text, url)? {
        Playlist::Media(m) => (vec![], url.to_string(), m),
        Playlist::Master(vs) => {
            let best = vs[0].url.clone();
            let t = get_text(client, req, &best).await?;
            match parse(&t, &best)? {
                Playlist::Media(m) => (vs, best, m),
                Playlist::Master(_) => return Err(tr!("不支持嵌套的主播放列表").into()),
            }
        }
    };
    let info = HlsInfo {
        variants,
        media_url,
        segments: media.segments.len(),
        duration: media.duration(),
        encrypted: media.encrypted(),
        fmp4: media.map.is_some(),
        drm: media.drm,
        live: !media.endlist,
    };
    Ok((info, media))
}

/// 用于文件名的主干：index/playlist 等无意义名称时取上级目录名
pub fn stem_for(url: &str, hint: Option<&str>) -> String {
    if let Some(h) = hint.map(str::trim).filter(|h| !h.is_empty()) {
        let h = h.rsplit_once('.').filter(|(_, e)| e.len() <= 5).map(|(s, _)| s).unwrap_or(h);
        if !h.is_empty() {
            return h.to_string();
        }
    }
    let Ok(u) = url::Url::parse(url) else { return "video".into() };
    let segs: Vec<String> = u
        .path_segments()
        .map(|s| s.filter(|x| !x.is_empty()).map(|x| percent_encoding::percent_decode_str(x).decode_utf8_lossy().into_owned()).collect())
        .unwrap_or_default();
    let generic = ["index", "playlist", "master", "prog_index", "chunklist", "video", "main", "stream", "mono", "media"];
    for s in segs.iter().rev() {
        let stem = s.rsplit_once('.').map(|(a, _)| a).unwrap_or(s);
        if !stem.is_empty() && !generic.iter().any(|g| stem.eq_ignore_ascii_case(g)) && !stem.starts_with("chunklist") {
            return stem.to_string();
        }
    }
    "video".into()
}

/// 查找 ffmpeg：设置中的路径 → PATH → 程序目录
pub fn find_ffmpeg(setting: &str) -> Option<PathBuf> {
    let s = setting.trim();
    if !s.is_empty() {
        let p = PathBuf::from(s);
        return p.is_file().then_some(p);
    }
    let in_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("ffmpeg.exe"))
        .find(|p| p.is_file());
    in_path.or_else(|| std::env::current_exe().ok()?.parent().map(|d| d.join("ffmpeg.exe")).filter(|p| p.is_file()))
}

/// TS 无损转封装为 MP4
fn remux(ffmpeg: &Path, input: &Path, output: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let st = std::process::Command::new(ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "mpegts", "-i"])
        .arg(input)
        .args(["-c", "copy", "-bsf:a", "aac_adtstoasc"])
        .arg(output)
        .creation_flags(0x0800_0000)
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() && output.is_file() { Ok(()) } else { Err(trf!("ffmpeg 退出码 {}", format!("{:?}", st.code()))) }
}

struct Ctx {
    client: reqwest::Client,
    req: AddReq,
    m: Arc<Manager>,
    t: Arc<Task>,
    tok: CancellationToken,
    keys: tokio::sync::Mutex<HashMap<String, [u8; 16]>>,
}

async fn fetch_key(c: &Ctx, uri: &str) -> Result<[u8; 16], String> {
    let mut keys = c.keys.lock().await;
    if let Some(k) = keys.get(uri) {
        return Ok(*k);
    }
    let resp = with_headers(c.client.get(uri), &c.req).send().await.map_err(err_str)?;
    let st = resp.status().as_u16();
    if !(200..300).contains(&st) {
        return Err(trf!("获取解密密钥失败 (HTTP {})", st));
    }
    let b = resp.bytes().await.map_err(err_str)?;
    let k: [u8; 16] = b.as_ref().try_into().map_err(|_| trf!("解密密钥长度应为 16 字节，实际 {}", b.len()))?;
    keys.insert(uri.to_string(), k);
    Ok(k)
}

/// 下载一个分片（含重试、限速、停滞检测、解密）
async fn fetch_seg(c: &Ctx, url: &str, range: Option<(u64, u64)>, key: Option<([u8; 16], [u8; 16])>) -> Result<Vec<u8>, String> {
    let stall = Duration::from_millis(super::task::STALL_MS.load(Relaxed));
    let mut fails = 0u32;
    loop {
        if c.tok.is_cancelled() {
            return Err(CANCELLED.into());
        }
        let mut got = 0u64;
        let attempt: Result<Vec<u8>, (String, bool)> = async {
            let mut rb = with_headers(c.client.get(url), &c.req);
            if let Some((off, len)) = range {
                rb = rb.header(reqwest::header::RANGE, format!("bytes={off}-{}", off + len - 1));
            }
            let mut resp = tokio::select! {
                r = rb.send() => r.map_err(|e| (err_str(e), false))?,
                _ = c.tok.cancelled() => return Err((CANCELLED.into(), true)),
            };
            let st = resp.status().as_u16();
            if !(200..300).contains(&st) {
                let fatal = matches!(st, 401 | 403 | 404 | 410);
                return Err((trf!("分片下载失败 (HTTP {})", st), fatal));
            }
            let mut buf = Vec::with_capacity(resp.content_length().unwrap_or(0).min(64 << 20) as usize);
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = c.tok.cancelled() => return Err((CANCELLED.into(), true)),
                    r = tokio::time::timeout(stall, resp.chunk()) => match r {
                        Ok(r) => r.map_err(|e| (err_str(e), false))?,
                        Err(_) => return Err((tr!("连接停滞").into(), false)),
                    },
                };
                let Some(b) = chunk else { break };
                for piece in b.chunks(64 << 10) {
                    if c.m.limiter.limited() {
                        c.m.limiter.acquire(piece.len()).await;
                    }
                    c.t.downloaded.fetch_add(piece.len() as u64, Relaxed);
                    got += piece.len() as u64;
                    buf.extend_from_slice(piece);
                }
            }
            if let Some((k, iv)) = key {
                decrypt(&k, &iv, &mut buf).map_err(|e| (e, true))?;
            }
            Ok(buf)
        }
        .await;
        match attempt {
            Ok(b) => return Ok(b),
            Err((e, fatal)) => {
                c.t.downloaded.fetch_sub(got, Relaxed);
                if e == CANCELLED || c.tok.is_cancelled() {
                    return Err(CANCELLED.into());
                }
                fails += 1;
                if fails > if fatal { 2 } else { MAX_RETRY } {
                    return Err(e);
                }
                let wait = backoff(fails, 20_000);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = c.tok.cancelled() => return Err(CANCELLED.into()),
                }
            }
        }
    }
}

/// 执行 HLS 任务。调用前任务名称与目录已确定。
pub async fn run(m: &Arc<Manager>, t: &Arc<Task>, tok: &CancellationToken, client: reqwest::Client, info: HlsInfo) -> Result<(), String> {
    if info.drm {
        return Err(tr!("该视频受 DRM 保护，无法下载").into());
    }
    let req = t.req();
    // 续传时沿用当初选定的清晰度；新任务用探测结果（默认最高码率）
    let prev = t.meta.lock().unwrap().hls.clone();
    let media_url = prev.as_ref().map(|s| s.media_url.clone()).filter(|u| !u.is_empty()).unwrap_or(info.media_url.clone());
    let text = tokio::select! {
        r = get_text(&client, &req, &media_url) => r?,
        _ = tok.cancelled() => return Err(CANCELLED.into()),
    };
    let media = match parse(&text, &media_url)? {
        Playlist::Media(md) => md,
        Playlist::Master(_) => return Err(tr!("不支持嵌套的主播放列表").into()),
    };
    if media.drm {
        return Err(tr!("该视频受 DRM 保护，无法下载").into());
    }
    if !media.endlist {
        return Err(tr!("这是直播流，暂不支持下载").into());
    }
    if media.segments.is_empty() {
        return Err(tr!("播放列表中没有分片").into());
    }
    let total = media.segments.len() as u32;

    // 续传判断：分片数一致且临时文件存在
    let part = t.part_path();
    let dir = t.meta.lock().unwrap().dir.clone();
    std::fs::create_dir_all(&dir).map_err(|e| trf!("无法创建目录: {}", e))?;
    let resume = prev.as_ref().filter(|s| s.total == total && s.media_url == media_url && part.exists() && s.done < total).cloned();
    let mut st = resume.unwrap_or(HlsState {
        media_url: media_url.clone(),
        total,
        fmp4: media.map.is_some(),
        encrypted: media.encrypted(),
        duration: media.duration(),
        ..Default::default()
    });
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(st.done == 0 && !st.map_done).open(&part).map_err(|e| trf!("无法打开文件: {}", e))?;
    f.set_len(st.bytes).map_err(|e| e.to_string())?;
    let file = Arc::new(f);
    t.downloaded.store(st.bytes, Relaxed);
    t.seg_total.store(total, Relaxed);
    t.seg_done.store(st.done, Relaxed);
    t.meta.lock().unwrap().hls = Some(st.clone());
    m.mark_dirty();
    m.repaint();

    let c = Arc::new(Ctx { client, req, m: m.clone(), t: t.clone(), tok: tok.clone(), keys: Default::default() });

    // 初始化段（fMP4）
    if let Some((url, range)) = media.map.clone()
        && !st.map_done
    {
        let data = fetch_seg(&c, &url, range, None).await?;
        let (fc, off) = (file.clone(), st.bytes);
        let len = data.len() as u64;
        tokio::task::spawn_blocking(move || write_at(&fc, &data, off)).await.map_err(|e| e.to_string())??;
        st.bytes += len;
        st.map_done = true;
        t.meta.lock().unwrap().hls = Some(st.clone());
    }

    // 分片：并发下载、按顺序写入（buffered 保证顺序并限制在途数量）
    let conns = t.conns.clamp(1, 32) as usize;
    let segs: Vec<_> = media.segments[st.done as usize..].to_vec();
    let mut stream = futures_util::stream::iter(segs.into_iter().map(|s| {
        let c = c.clone();
        async move {
            let key = match &s.key {
                Some(k) => Some((fetch_key(&c, &k.uri).await?, segment_iv(&s))),
                None => None,
            };
            fetch_seg(&c, &s.url, s.range, key).await
        }
    }))
    .buffered(conns);
    let mut last_save = std::time::Instant::now();
    while let Some(r) = stream.next().await {
        let data = r?;
        let (fc, off) = (file.clone(), st.bytes);
        let len = data.len() as u64;
        tokio::task::spawn_blocking(move || write_at(&fc, &data, off)).await.map_err(|e| e.to_string())??;
        st.bytes += len;
        st.done += 1;
        t.seg_done.store(st.done, Relaxed);
        t.meta.lock().unwrap().hls = Some(st.clone());
        if last_save.elapsed() > Duration::from_secs(2) {
            m.mark_dirty();
            last_save = std::time::Instant::now();
        }
    }
    drop(stream);
    if tok.is_cancelled() {
        return Err(CANCELLED.into());
    }
    drop(file);
    t.downloaded.store(st.bytes, Relaxed);

    // 收尾：fMP4 直接为 .mp4；TS 尝试用 ffmpeg 转封装为 .mp4，失败则保存为 .ts
    let (dir, name) = {
        let meta = t.meta.lock().unwrap();
        (meta.dir.clone(), meta.name.clone())
    };
    let stem = name.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(name.clone());
    let (want_mp4, ffmpeg) = {
        let s = m.settings.read().unwrap();
        (s.hls_mp4, find_ffmpeg(&s.ffmpeg))
    };
    let mut note = None;
    let final_name = if st.fmp4 {
        let n = super::unique_name(&format!("{stem}.mp4"), |n| dir.join(n).exists());
        std::fs::rename(&part, dir.join(&n)).map_err(|e| trf!("重命名失败: {}", e))?;
        n
    } else {
        let mp4 = super::unique_name(&format!("{stem}.mp4"), |n| dir.join(n).exists());
        let converted = match (want_mp4, &ffmpeg) {
            (true, Some(ff)) => {
                let (ff, input, out) = (ff.clone(), part.clone(), dir.join(&mp4));
                let r = tokio::task::spawn_blocking(move || remux(&ff, &input, &out)).await.map_err(|e| e.to_string())?;
                match r {
                    Ok(()) => true,
                    Err(e) => {
                        let _ = std::fs::remove_file(dir.join(&mp4));
                        note = Some(trf!("转换 MP4 失败（{}），已保存为 .ts", e));
                        false
                    }
                }
            }
            (true, None) => {
                note = Some(tr!("未找到 ffmpeg，已保存为 .ts").into());
                false
            }
            (false, _) => false,
        };
        if converted {
            let _ = std::fs::remove_file(&part);
            mp4
        } else {
            let ts = super::unique_name(&format!("{stem}.ts"), |n| dir.join(n).exists());
            std::fs::rename(&part, dir.join(&ts)).map_err(|e| trf!("重命名失败: {}", e))?;
            ts
        }
    };
    let size = std::fs::metadata(dir.join(&final_name)).map(|md| md.len()).unwrap_or(st.bytes);
    st.note = note;
    let mut meta = t.meta.lock().unwrap();
    meta.name = final_name;
    meta.size = Some(size);
    meta.finished = now();
    meta.hls = Some(st);
    t.downloaded.store(size, Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://cdn.example.com/v/1080/index.m3u8?token=abc";

    #[test]
    fn master() {
        let t = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\nlow/index.m3u8\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=4500000,RESOLUTION=1920x1080\nhttps://other.example.com/hi.m3u8\n";
        let Playlist::Master(v) = parse(t, BASE).unwrap() else { panic!() };
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].url, "https://other.example.com/hi.m3u8");
        assert_eq!(v[0].label(), "1080p  4.5 Mbps");
        assert_eq!(v[1].url, "https://cdn.example.com/v/1080/low/index.m3u8");
        assert_eq!(v[1].resolution.as_deref(), Some("640x360"));
    }

    #[test]
    fn media_full() {
        let t = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
                 #EXT-X-KEY:METHOD=AES-128,URI=\"../key?id=1\"\n#EXTINF:4.0,\nseg0.m4s\n\
                 #EXT-X-KEY:METHOD=AES-128,URI=\"k2\",IV=0x000102030405060708090a0b0c0d0e0f\n#EXTINF:4.5,title\n/abs/seg1.m4s\n\
                 #EXT-X-KEY:METHOD=NONE\n#EXT-X-BYTERANGE:1000@0\n#EXTINF:2,\nall.ts\n#EXT-X-BYTERANGE:500\n#EXTINF:2,\nall.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media(m) = parse(t, BASE).unwrap() else { panic!() };
        assert!(m.endlist && !m.drm && m.encrypted());
        assert_eq!(m.map, Some(("https://cdn.example.com/v/1080/init.mp4".into(), Some((0, 720)))));
        assert_eq!(m.segments.len(), 4);
        let s = &m.segments;
        assert_eq!(s[0].url, "https://cdn.example.com/v/1080/seg0.m4s");
        assert_eq!(s[0].seq, 10);
        assert_eq!(s[0].key.as_ref().unwrap().uri, "https://cdn.example.com/v/key?id=1");
        assert_eq!(segment_iv(&s[0]), 10u128.to_be_bytes());
        assert_eq!(s[1].url, "https://cdn.example.com/abs/seg1.m4s");
        assert_eq!(segment_iv(&s[1])[15], 0x0f);
        assert_eq!(s[1].duration, 4.5);
        assert!(s[2].key.is_none());
        assert_eq!(s[2].range, Some((0, 1000)));
        assert_eq!(s[3].range, Some((1000, 500)), "无偏移的 BYTERANGE 应接续上一段");
        assert_eq!(m.duration(), 12.5);
    }

    #[test]
    fn drm_live_invalid() {
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media(m) = parse(drm, BASE).unwrap() else { panic!() };
        assert!(m.drm);
        let widevine = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"x\",KEYFORMAT=\"urn:uuid:edef8ba9\"\n#EXTINF:4,\na.ts\n";
        let Playlist::Media(m) = parse(widevine, BASE).unwrap() else { panic!() };
        assert!(m.drm && !m.endlist);
        assert!(parse("<html>", BASE).is_err());
    }

    #[test]
    fn aes_roundtrip() {
        use aes::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
        let (key, iv) = ([7u8; 16], [9u8; 16]);
        let plain: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let mut enc = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into()).encrypt_padded_vec::<Pkcs7>(&plain);
        decrypt(&key, &iv, &mut enc).unwrap();
        assert_eq!(enc, plain);
        let mut bad = vec![1u8; 32];
        assert!(decrypt(&key, &iv, &mut bad).is_err());
    }

    #[test]
    fn names() {
        assert_eq!(stem_for("https://a.com/movies/My%20Film/index.m3u8", None), "My Film");
        assert_eq!(stem_for("https://a.com/x/chunklist_w123.m3u8", None), "x");
        assert_eq!(stem_for("https://a.com/x/clip.m3u8", None), "clip");
        assert_eq!(stem_for("https://a.com/index.m3u8", Some("网页标题.mp4")), "网页标题");
        assert_eq!(stem_for("https://a.com/index.m3u8", None), "video");
        assert!(looks_like_hls("https://a.com/a.M3U8?x=1", ""));
        assert!(looks_like_hls("https://a.com/play", "application/vnd.apple.mpegurl"));
        assert!(!looks_like_hls("https://a.com/a.mp4", "video/mp4"));
    }
}
