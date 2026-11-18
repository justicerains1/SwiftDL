//! 单任务执行：探测 → 预分配 → 多连接定位写入 → 动态分段 → 重命名。
use super::probe::{self, err_str};
use super::{Manager, Seg, SegPos, Status, Task, now};
use std::fs::{File, OpenOptions};
use std::os::windows::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::Ordering::*;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// 动态拆分时每块最小 1 MiB
const MIN_SPLIT: u64 = 1 << 20;
/// 每连接写缓冲 1 MiB
const BUF: usize = 1 << 20;
/// 限速粒度 64 KiB
const PIECE: usize = 64 << 10;
const MAX_RETRY: u32 = 8;
const CANCELLED: &str = "cancelled";

pub async fn run(m: Arc<Manager>, t: Arc<Task>) {
    let tok = t.cancel.lock().unwrap().clone();
    let r = run_inner(&m, &t, &tok).await;
    t.active_conns.store(0, Relaxed);
    {
        let mut meta = t.meta.lock().unwrap();
        match r {
            Ok(()) => {
                meta.error = None;
                meta.finished = now();
                t.set_status(Status::Done);
            }
            Err(_) if tok.is_cancelled() => t.set_status(Status::Paused),
            Err(e) => {
                meta.error = Some(e);
                t.set_status(Status::Error);
            }
        }
    }
    m.save();
    m.schedule();
    m.repaint();
}

struct Ctx {
    client: reqwest::Client,
    t: Arc<Task>,
    m: Arc<Manager>,
    file: Arc<File>,
    tok: CancellationToken,
    resumable: bool,
}

async fn run_inner(m: &Arc<Manager>, t: &Arc<Task>, tok: &CancellationToken) -> Result<(), String> {
    let client = m.client();
    let info = tokio::select! {
        r = probe::probe(&client, &t.req) => r?,
        _ = tok.cancelled() => return Err(CANCELLED.into()),
    };

    // 决定是全新下载还是续传
    let (fresh, resumable, size) = {
        let mut meta = t.meta.lock().unwrap();
        if meta.name.is_empty() {
            let dir = meta.dir.clone();
            drop(meta);
            let n = m.reserve_name(t.id, &dir, &info.filename);
            meta = t.meta.lock().unwrap();
            meta.name = n;
        }
        let part_exists = meta.dir.join(format!("{}.{}", meta.name, super::PART_EXT)).exists();
        let mut segs = t.segs.lock().unwrap();
        let changed = meta.size != info.size
            || (meta.validator.is_some() && info.validator.is_some() && meta.validator != info.validator);
        let fresh = !meta.probed || changed || !info.resumable || segs.is_empty() || !part_exists;
        if fresh {
            meta.size = info.size;
            meta.resumable = info.resumable;
            meta.validator = info.validator.clone();
            meta.probed = true;
            *segs = initial_segs(info.size, info.resumable, t.conns);
            t.downloaded.store(0, Relaxed);
        }
        meta.error = None;
        (fresh, meta.resumable, meta.size)
    };
    m.mark_dirty();
    m.repaint();

    let part = t.part_path();
    let dir = t.meta.lock().unwrap().dir.clone();
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建目录: {e}"))?;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(fresh)
        .open(&part)
        .map_err(|e| format!("无法打开文件: {e}"))?;
    if fresh && let Some(sz) = size {
        f.set_len(sz).map_err(|e| format!("磁盘空间不足或无法预分配: {e}"))?;
    }

    let wtok = tok.child_token();
    let ctx = Arc::new(Ctx { client, t: t.clone(), m: m.clone(), file: Arc::new(f), tok: wtok.clone(), resumable });
    let mut set: JoinSet<Result<(), String>> = JoinSet::new();
    let max = t.conns.max(1) as usize;
    while set.len() < max {
        let Some(s) = next_seg(t, resumable && size.is_some()) else { break };
        spawn_worker(&mut set, &ctx, s);
    }

    let mut failure = None;
    while let Some(r) = set.join_next().await {
        match r {
            Ok(Ok(())) => {
                if !wtok.is_cancelled()
                    && let Some(s) = next_seg(t, resumable && size.is_some())
                {
                    spawn_worker(&mut set, &ctx, s);
                }
            }
            Ok(Err(e)) => {
                if failure.is_none() && e != CANCELLED {
                    failure = Some(e);
                }
                wtok.cancel();
            }
            Err(e) => {
                failure.get_or_insert(e.to_string());
                wtok.cancel();
            }
        }
    }
    if tok.is_cancelled() {
        return Err(CANCELLED.into());
    }
    if let Some(e) = failure {
        return Err(e);
    }

    // 校验与收尾
    let remaining: u64 = t
        .segs
        .lock()
        .unwrap()
        .iter()
        .map(|s| {
            let p = s.st.lock().unwrap();
            p.end.saturating_sub(s.written.load(Acquire))
        })
        .sum();
    if size.is_some() && remaining != 0 {
        return Err(format!("数据不完整，剩余 {remaining} 字节"));
    }
    if size.is_none() {
        let len = t.downloaded.load(Relaxed);
        ctx.file.set_len(len).map_err(|e| e.to_string())?;
        t.meta.lock().unwrap().size = Some(len);
    }
    drop(ctx);
    let (dir, name) = {
        let m = t.meta.lock().unwrap();
        (m.dir.clone(), m.name.clone())
    };
    let final_name = super::unique_name(&name, |n| dir.join(n).exists());
    std::fs::rename(&part, dir.join(&final_name)).map_err(|e| format!("重命名失败: {e}"))?;
    t.meta.lock().unwrap().name = final_name;
    t.segs.lock().unwrap().clear();
    Ok(())
}

fn initial_segs(size: Option<u64>, resumable: bool, conns: u32) -> Vec<Arc<Seg>> {
    match size {
        Some(0) => vec![],
        Some(sz) if resumable => {
            let n = (sz / MIN_SPLIT).clamp(1, conns.max(1) as u64);
            let step = sz / n;
            (0..n).map(|i| Seg::new(i * step, if i == n - 1 { sz } else { (i + 1) * step })).collect()
        }
        Some(sz) => vec![Seg::new(0, sz)],
        None => vec![Seg::new(0, u64::MAX)],
    }
}

/// 优先取未开工的段；没有则把剩余最多的段对半拆分（IDM 式动态分段）
fn next_seg(t: &Task, can_split: bool) -> Option<Arc<Seg>> {
    let mut segs = t.segs.lock().unwrap();
    let rem = |s: &Seg| {
        let p = s.st.lock().unwrap();
        p.end.saturating_sub(p.pos)
    };
    if let Some(s) = segs.iter().find(|s| !s.busy.load(Acquire) && rem(s) > 0) {
        s.busy.store(true, Release);
        return Some(s.clone());
    }
    if !can_split {
        return None;
    }
    let big = segs.iter().max_by_key(|s| rem(s))?.clone();
    let new = {
        let mut p = big.st.lock().unwrap();
        let r = p.end.saturating_sub(p.pos);
        if r < MIN_SPLIT * 2 {
            return None;
        }
        let mid = p.pos + r / 2;
        let new = Seg::new(mid, p.end);
        p.end = mid;
        new
    };
    new.busy.store(true, Release);
    segs.push(new.clone());
    Some(new)
}

fn spawn_worker(set: &mut JoinSet<Result<(), String>>, ctx: &Arc<Ctx>, seg: Arc<Seg>) {
    let ctx = ctx.clone();
    set.spawn(async move {
        let r = worker(&ctx, &seg).await;
        seg.busy.store(false, Release);
        r
    });
}

async fn worker(c: &Ctx, seg: &Seg) -> Result<(), String> {
    let mut fails = 0u32;
    loop {
        if c.tok.is_cancelled() {
            return Err(CANCELLED.into());
        }
        let SegPos { pos, end } = {
            // 以已落盘位置为准重新对齐（上次写入失败时避免留洞）
            let mut p = seg.st.lock().unwrap();
            let w = seg.written.load(Acquire);
            if p.pos > w {
                c.t.downloaded.fetch_sub(p.pos - w, Relaxed);
                p.pos = w;
            }
            *p
        };
        if pos >= end {
            return Ok(());
        }
        if !c.resumable && pos > 0 {
            // 不支持续传：只能从头再来
            *seg.st.lock().unwrap() = SegPos { pos: 0, end };
            seg.written.store(0, Release);
            c.t.downloaded.store(0, Relaxed);
        }
        c.t.active_conns.fetch_add(1, Relaxed);
        let r = fetch(c, seg).await;
        c.t.active_conns.fetch_sub(1, Relaxed);
        let after = seg.st.lock().unwrap().pos;
        match r {
            Ok(()) if end == u64::MAX => {
                seg.st.lock().unwrap().end = after; // 大小未知：流结束即完成
                return Ok(());
            }
            Ok(()) if after >= seg.st.lock().unwrap().end => return Ok(()),
            _ if c.tok.is_cancelled() => return Err(CANCELLED.into()),
            r => {
                if after > pos {
                    fails = 0;
                }
                fails += 1;
                if fails > MAX_RETRY {
                    return Err(r.err().unwrap_or_else(|| "连接提前断开".into()));
                }
                let wait = Duration::from_secs((1u64 << fails.min(5)).min(30));
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = c.tok.cancelled() => return Err(CANCELLED.into()),
                }
            }
        }
    }
}

async fn fetch(c: &Ctx, seg: &Seg) -> Result<(), String> {
    let SegPos { pos, end } = *seg.st.lock().unwrap();
    let mut rb = probe::with_headers(c.client.get(&c.t.req.url), &c.t.req);
    if c.resumable {
        let last = if end == u64::MAX { String::new() } else { (end - 1).to_string() };
        rb = rb.header(reqwest::header::RANGE, format!("bytes={pos}-{last}"));
    }
    let mut resp = tokio::select! {
        r = rb.send() => r.map_err(err_str)?,
        _ = c.tok.cancelled() => return Err(CANCELLED.into()),
    };
    let st = resp.status().as_u16();
    if c.resumable {
        if st != 206 {
            return Err(format!("服务器拒绝分段请求 (HTTP {st})"));
        }
        let start = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim_start_matches("bytes ").split('-').next()?.trim().parse::<u64>().ok());
        if start != Some(pos) {
            return Err("服务器返回的分段位置不符".into());
        }
    } else if !(200..300).contains(&st) {
        return Err(format!("HTTP {st}"));
    }

    let mut buf: Vec<u8> = Vec::with_capacity(BUF);
    let mut off = pos;
    let res: Result<(), String> = loop {
        let chunk = tokio::select! {
            biased;
            _ = c.tok.cancelled() => break Err(CANCELLED.into()),
            r = resp.chunk() => r,
        };
        let chunk = match chunk {
            Ok(Some(b)) => b,
            Ok(None) => break Ok(()),
            Err(e) => break Err(err_str(e)),
        };
        // 大块切成小片：先取令牌再认领，限速平滑且速度显示不跳变
        let mut rest: &[u8] = &chunk;
        let mut done = false;
        while !rest.is_empty() && !done {
            let piece = rest.len().min(PIECE);
            if c.m.limiter.limited() {
                tokio::select! {
                    biased;
                    _ = c.tok.cancelled() => break,
                    _ = c.m.limiter.acquire(piece) => {}
                }
            }
            // 在锁内认领字节：与动态拆分互斥，保证不越界
            let take = {
                let mut p = seg.st.lock().unwrap();
                let take = (p.end - p.pos).min(piece as u64) as usize;
                p.pos += take as u64;
                done = p.pos >= p.end;
                take
            };
            c.t.downloaded.fetch_add(take as u64, Relaxed);
            buf.extend_from_slice(&rest[..take]);
            rest = &rest[piece..];
            if buf.len() >= BUF || done {
                buf = flush(c, seg, buf, off).await?;
                off = seg.written.load(Acquire);
            }
        }
        if done {
            break Ok(());
        }
        if c.tok.is_cancelled() {
            break Err(CANCELLED.into());
        }
    };
    if !buf.is_empty() {
        flush(c, seg, buf, off).await?;
    }
    res
}

/// 在阻塞线程池中做定位写入，写完后推进 written 并归还缓冲区
async fn flush(c: &Ctx, seg: &Seg, mut buf: Vec<u8>, off: u64) -> Result<Vec<u8>, String> {
    if buf.is_empty() {
        return Ok(buf);
    }
    let file = c.file.clone();
    let (r, b) = tokio::task::spawn_blocking(move || {
        let mut done = 0;
        let r = loop {
            if done == buf.len() {
                break Ok(());
            }
            match file.seek_write(&buf[done..], off + done as u64) {
                Ok(0) => break Err("写入 0 字节".to_string()),
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break Err(format!("写入失败: {e}")),
            }
        };
        (r, buf)
    })
    .await
    .map_err(|e| e.to_string())?;
    r?;
    seg.written.store(off + b.len() as u64, Release);
    buf = b;
    buf.clear();
    Ok(buf)
}
