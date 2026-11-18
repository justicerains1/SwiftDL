//! 单任务执行：探测 → 稀疏预分配 → 多连接定位写入 → 动态分段 → 自适应并发 → 校验 → 重命名。
use super::probe::{self, err_str};
use super::{Manager, Seg, SegPos, Status, Task, kind, now, sha256_file};
use std::fs::{File, OpenOptions};
use std::os::windows::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// 动态拆分时每块最小 1 MiB
const MIN_SPLIT: u64 = 1 << 20;
/// 每连接写缓冲 1 MiB
const BUF: usize = 1 << 20;
/// 限速粒度 64 KiB
const PIECE: usize = 64 << 10;
/// 单个 worker 连续失败上限
const MAX_RETRY: u32 = 8;
/// 同一分段允许失败的 worker 轮数，超过则判定任务出错
const MAX_SEG_ROUNDS: u32 = 3;
/// 限流后平稳多久恢复一个连接（秒）
const RESTORE_AFTER: u64 = 30;
const CANCELLED: &str = "cancelled";

/// 连接停滞超时（毫秒），测试中可调小
pub static STALL_MS: AtomicU64 = AtomicU64::new(15_000);

pub async fn run(m: Arc<Manager>, t: Arc<Task>) {
    let tok = t.cancel.lock().unwrap().clone();
    let mut r = run_inner(&m, &t, &tok).await;
    t.active_conns.store(0, Relaxed);
    if r.is_ok() {
        r = verify_hash(&t).await;
    }
    {
        let mut meta = t.meta.lock().unwrap();
        match r {
            Ok(()) => {
                meta.error = None;
                meta.finished = now();
                meta.retries = 0;
                t.set_status(Status::Done);
            }
            Err(_) if tok.is_cancelled() => t.set_status(Status::Paused),
            Err(e) => {
                if e.starts_with("SHA-256") {
                    meta.retries = u32::MAX; // 校验失败不自动重试
                }
                meta.error = Some(e);
                meta.error_at = now();
                t.set_status(Status::Error);
            }
        }
    }
    m.save();
    m.schedule();
    m.repaint();
}

/// 用户填写了期望 SHA-256 时，完成后校验
async fn verify_hash(t: &Arc<Task>) -> Result<(), String> {
    let Some(expect) = t.meta.lock().unwrap().sha256.clone() else { return Ok(()) };
    let path = t.final_path();
    let got = tokio::task::spawn_blocking(move || sha256_file(&path))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("SHA-256 计算失败：{e}"))?;
    let ok = got.eq_ignore_ascii_case(&expect);
    t.meta.lock().unwrap().hash = Some(got);
    if ok { Ok(()) } else { Err("SHA-256 校验失败，文件可能已损坏".into()) }
}

struct Ctx {
    client: reqwest::Client,
    t: Arc<Task>,
    m: Arc<Manager>,
    file: Arc<File>,
    tok: CancellationToken,
    resumable: bool,
    workers: AtomicU32,
}

/// worker 结束原因
enum End {
    Done,
    /// 因限流主动让出连接
    Yield,
    Fail(String),
    /// 不可恢复的错误（如 403/404），立即结束任务
    Fatal(String),
    Cancelled,
}

struct FErr {
    msg: String,
    throttle: bool,
    fatal: bool,
}

impl FErr {
    fn net(msg: String) -> Self {
        Self { msg, throttle: false, fatal: false }
    }
}

async fn run_inner(m: &Arc<Manager>, t: &Arc<Task>, tok: &CancellationToken) -> Result<(), String> {
    let client = m.client();
    let req = t.req();
    let info = tokio::select! {
        r = probe::probe(&client, &req) => r?,
        _ = tok.cancelled() => return Err(CANCELLED.into()),
    };

    // 决定是全新下载还是续传
    let (fresh, resumable, size) = {
        let mut meta = t.meta.lock().unwrap();
        if meta.name.is_empty() {
            let mut dir = meta.dir.clone();
            if meta.auto_dir && m.settings.read().unwrap().categorize {
                dir = dir.join(kind::category(&info.filename).folder());
            }
            drop(meta);
            let n = m.reserve_name(t.id, &dir, &info.filename);
            meta = t.meta.lock().unwrap();
            meta.name = n;
            meta.dir = dir;
        }
        let part_exists = meta.dir.join(format!("{}.{}", meta.name, super::PART_EXT)).exists();
        let mut segs = t.segs.lock().unwrap();
        // 更新过链接时只要求大小一致，不比较 ETag（不同镜像的 ETag 不同）
        let validator_changed =
            !meta.relink && meta.validator.is_some() && info.validator.is_some() && meta.validator != info.validator;
        let changed = meta.size != info.size || validator_changed;
        let fresh = !meta.probed || changed || !info.resumable || segs.is_empty() || !part_exists;
        if fresh {
            meta.size = info.size;
            meta.resumable = info.resumable;
            meta.probed = true;
            *segs = initial_segs(info.size, info.resumable, t.conns);
            t.downloaded.store(0, Relaxed);
        }
        meta.validator = info.validator.clone();
        meta.relink = false;
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
    let mut sparse = false;
    if fresh && let Some(sz) = size {
        // 稀疏文件：多连接在远端偏移写入时，NTFS 无需先同步补零整个空洞
        sparse = sz > 0 && t.segs.lock().unwrap().len() > 1 && set_sparse(&f, true);
        f.set_len(sz).map_err(|e| format!("磁盘空间不足或无法预分配: {e}"))?;
    }

    let wtok = tok.child_token();
    let ctx = Arc::new(Ctx {
        client,
        t: t.clone(),
        m: m.clone(),
        file: Arc::new(f),
        tok: wtok.clone(),
        resumable,
        workers: AtomicU32::new(0),
    });
    let can_split = resumable && size.is_some();
    t.conn_limit.store(t.conns.max(1), Relaxed);
    let mut set: JoinSet<(End, Arc<Seg>)> = JoinSet::new();
    let top_up = |set: &mut JoinSet<(End, Arc<Seg>)>| {
        while !wtok.is_cancelled() && set.len() < t.conn_limit.load(Relaxed).max(1) as usize {
            let Some(s) = next_seg(t, can_split) else { break };
            spawn_worker(set, &ctx, s);
        }
    };
    top_up(&mut set);

    let mut failure: Option<String> = None;
    let mut last_err: Option<String> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            r = set.join_next() => match r {
                None => break,
                Some(Ok((End::Done | End::Yield, _))) => top_up(&mut set),
                Some(Ok((End::Fail(e), seg))) => {
                    // 只释放该分段，由其他连接接手；同一分段反复失败才判定任务出错
                    if seg.fail_rounds.fetch_add(1, Relaxed) + 1 >= MAX_SEG_ROUNDS {
                        failure.get_or_insert(e);
                        wtok.cancel();
                    } else {
                        last_err = Some(e);
                        top_up(&mut set);
                    }
                }
                Some(Ok((End::Fatal(e), _))) => {
                    failure.get_or_insert(e);
                    wtok.cancel();
                }
                Some(Ok((End::Cancelled, _))) => {}
                Some(Err(e)) => {
                    failure.get_or_insert(e.to_string());
                    wtok.cancel();
                }
            },
            _ = tick.tick() => {
                // 限流平稳一段时间后逐步恢复连接数
                let lim = t.conn_limit.load(Relaxed);
                if lim < t.conns && now().saturating_sub(t.throttled_at.load(Relaxed)) >= RESTORE_AFTER {
                    t.conn_limit.store(lim + 1, Relaxed);
                }
                top_up(&mut set);
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
        return Err(last_err.unwrap_or_else(|| format!("数据不完整，剩余 {remaining} 字节")));
    }
    if size.is_none() {
        let len = t.downloaded.load(Relaxed);
        ctx.file.set_len(len).map_err(|e| e.to_string())?;
        t.meta.lock().unwrap().size = Some(len);
    }
    if sparse {
        set_sparse(&ctx.file, false); // 已无空洞，恢复为普通文件
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

/// 设置/取消 NTFS 稀疏属性；不支持的文件系统返回 false
pub fn set_sparse(f: &File, on: bool) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{FILE_SET_SPARSE_BUFFER, FSCTL_SET_SPARSE};
    let mut input = FILE_SET_SPARSE_BUFFER { SetSparse: on as _ };
    let mut ret = 0u32;
    unsafe {
        DeviceIoControl(
            f.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            &mut input as *mut _ as *const _,
            std::mem::size_of::<FILE_SET_SPARSE_BUFFER>() as u32,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
        ) != 0
    }
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

fn spawn_worker(set: &mut JoinSet<(End, Arc<Seg>)>, ctx: &Arc<Ctx>, seg: Arc<Seg>) {
    let ctx = ctx.clone();
    ctx.workers.fetch_add(1, Relaxed);
    set.spawn(async move {
        let r = worker(&ctx, &seg).await;
        seg.busy.store(false, Release);
        ctx.workers.fetch_sub(1, Relaxed);
        (r, seg)
    });
}

async fn worker(c: &Ctx, seg: &Seg) -> End {
    let mut fails = 0u32;
    loop {
        if c.tok.is_cancelled() {
            return End::Cancelled;
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
            return End::Done;
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
        let err = match r {
            Ok(()) if end == u64::MAX => {
                seg.st.lock().unwrap().end = after; // 大小未知：流结束即完成
                return End::Done;
            }
            Ok(()) if after >= seg.st.lock().unwrap().end => return End::Done,
            _ if c.tok.is_cancelled() => return End::Cancelled,
            Ok(()) => FErr::net("连接提前断开".into()),
            Err(e) => e,
        };
        if err.fatal {
            return End::Fatal(err.msg);
        }
        if err.throttle {
            // 服务器限流：降低该任务的并发，多余的连接主动退出
            c.t.throttled_at.store(now(), Relaxed);
            let _ = c.t.conn_limit.try_update(Relaxed, Relaxed, |l| Some(l.saturating_sub(1).max(1)));
            if c.workers.load(Relaxed) > c.t.conn_limit.load(Relaxed) {
                return End::Yield;
            }
        }
        if after > pos {
            fails = 0;
        }
        fails += 1;
        if fails > MAX_RETRY {
            return End::Fail(err.msg);
        }
        let wait = Duration::from_millis((500u64 << fails.min(6)).min(30_000));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = c.tok.cancelled() => return End::Cancelled,
        }
    }
}

async fn fetch(c: &Ctx, seg: &Seg) -> Result<(), FErr> {
    let SegPos { pos, end } = *seg.st.lock().unwrap();
    let req = c.t.req();
    let mut rb = probe::with_headers(c.client.get(&req.url), &req);
    if c.resumable {
        let last = if end == u64::MAX { String::new() } else { (end - 1).to_string() };
        rb = rb.header(reqwest::header::RANGE, format!("bytes={pos}-{last}"));
    }
    let mut resp = tokio::select! {
        r = rb.send() => r.map_err(|e| FErr::net(err_str(e)))?,
        _ = c.tok.cancelled() => return Err(FErr::net(CANCELLED.into())),
    };
    let st = resp.status().as_u16();
    if matches!(st, 429 | 503) {
        return Err(FErr { msg: format!("服务器限流 (HTTP {st})"), throttle: true, fatal: false });
    }
    if (400..500).contains(&st) && st != 408 {
        let msg = match st {
            403 => "链接已失效或无权访问 (HTTP 403)，可右键更新下载链接".into(),
            404 | 410 => format!("文件不存在 (HTTP {st})，可右键更新下载链接"),
            _ => format!("HTTP {st}"),
        };
        return Err(FErr { msg, throttle: false, fatal: true });
    }
    if c.resumable {
        if st != 206 {
            return Err(FErr::net(format!("服务器拒绝分段请求 (HTTP {st})")));
        }
        let start = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim_start_matches("bytes ").split('-').next()?.trim().parse::<u64>().ok());
        if start != Some(pos) {
            return Err(FErr::net("服务器返回的分段位置不符".into()));
        }
    } else if !(200..300).contains(&st) {
        return Err(FErr::net(format!("HTTP {st}")));
    }

    let stall = Duration::from_millis(STALL_MS.load(Relaxed));
    let mut buf: Vec<u8> = Vec::with_capacity(BUF);
    let mut off = pos;
    let res: Result<(), String> = loop {
        let chunk = tokio::select! {
            biased;
            _ = c.tok.cancelled() => break Err(CANCELLED.into()),
            r = tokio::time::timeout(stall, resp.chunk()) => match r {
                Ok(r) => r,
                Err(_) => break Err("连接停滞，正在重连".into()),
            },
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
                buf = flush(c, seg, buf, off).await.map_err(FErr::net)?;
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
        flush(c, seg, buf, off).await.map_err(FErr::net)?;
    }
    res.map_err(FErr::net)
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
