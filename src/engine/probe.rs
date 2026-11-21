//! 资源探测：大小、是否支持分段、文件名、校验标识。
use super::AddReq;
use reqwest::header::{self, HeaderMap};

#[derive(Clone, Debug)]
pub struct Info {
    pub size: Option<u64>,
    pub resumable: bool,
    pub filename: String,
    pub validator: Option<String>,
    /// HLS 流媒体（m3u8）的探测结果
    pub hls: Option<super::hls::HlsInfo>,
    /// BT 种子信息
    pub bt: Option<super::bt::BtInfo>,
}

pub const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

pub fn with_headers(mut rb: reqwest::RequestBuilder, req: &AddReq) -> reqwest::RequestBuilder {
    if let Some(r) = req.referrer.as_deref().filter(|s| !s.is_empty()) {
        rb = rb.header(header::REFERER, r);
    }
    if let Some(c) = req.cookies.as_deref().filter(|s| !s.is_empty()) {
        rb = rb.header(header::COOKIE, c);
    }
    let ua = req.user_agent.as_deref().filter(|s| !s.is_empty()).unwrap_or(DEFAULT_UA);
    rb.header(header::USER_AGENT, ua)
}

pub async fn probe(client: &reqwest::Client, req: &AddReq) -> Result<Info, String> {
    if super::remote::is_remote(&req.url) {
        return super::remote::probe(req).await;
    }
    if super::hls::looks_like_hls(&req.url, "") {
        return probe_hls(client, req).await;
    }
    let resp = with_headers(client.get(&req.url), req)
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(err_str)?;
    let status = resp.status().as_u16();
    let h = resp.headers();
    // 扩展名不是 m3u8 但 Content-Type 表明是播放列表
    if (200..300).contains(&status) && super::hls::looks_like_hls("", &hval(h, header::CONTENT_TYPE).unwrap_or_default()) {
        drop(resp);
        return probe_hls(client, req).await;
    }
    let (size, resumable) = match status {
        206 => {
            let total = hval(h, header::CONTENT_RANGE)
                .and_then(|v| v.rsplit('/').next().and_then(|t| t.trim().parse::<u64>().ok()));
            (total, total.is_some())
        }
        200..=299 => (hval(h, header::CONTENT_LENGTH).and_then(|v| v.trim().parse().ok()), false),
        _ => return Err(format!("HTTP {status}")),
    };
    let filename = req
        .filename
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| hval(h, header::CONTENT_DISPOSITION).and_then(|v| disposition_name(&v)))
        .or_else(|| url_name(resp.url().as_str()))
        .or_else(|| url_name(&req.url))
        .map(|n| sanitize(&n))
        .unwrap_or_else(|| "download".into());
    let validator = hval(h, header::ETAG).or_else(|| hval(h, header::LAST_MODIFIED));
    Ok(Info { size, resumable, filename, validator, hls: None, bt: None })
}

/// HLS：解析播放列表（主列表时取最高码率的媒体列表），文件名先按 .mp4 预设，任务开始时再定扩展名
async fn probe_hls(client: &reqwest::Client, req: &AddReq) -> Result<Info, String> {
    let (info, _) = super::hls::load(client, req, &req.url).await?;
    let stem = super::hls::stem_for(&req.url, req.filename.as_deref());
    Ok(Info { size: None, resumable: false, filename: sanitize(&format!("{stem}.mp4")), validator: None, hls: Some(info), bt: None })
}

pub fn err_str(e: reqwest::Error) -> String {
    use std::error::Error;
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s.push_str(": ");
        s.push_str(&c.to_string());
        src = c.source();
    }
    cert_hint(s)
}

/// 证书错误加上易懂的说明（保留原文，其中的错误码用于识别）
pub fn cert_hint(s: String) -> String {
    if super::sys::is_cert_code_error(&s) {
        trf!("服务器证书不可信（自签名、已过期或与域名不符），可信任此站点后重试：{}", s)
    } else {
        s
    }
}

fn hval(h: &HeaderMap, k: header::HeaderName) -> Option<String> {
    h.get(k).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn disposition_name(v: &str) -> Option<String> {
    let parts: Vec<&str> = v.split(';').map(str::trim).collect();
    for p in &parts {
        if p.len() > 10 && p[..10].eq_ignore_ascii_case("filename*=") {
            let r = p[10..].trim_matches('"');
            let enc = r.splitn(3, '\'').nth(2).unwrap_or(r);
            return Some(decode(enc)).filter(|s| !s.is_empty());
        }
    }
    for p in &parts {
        if p.len() > 9 && p[..9].eq_ignore_ascii_case("filename=") {
            return Some(decode(p[9..].trim_matches('"'))).filter(|s| !s.is_empty());
        }
    }
    None
}

pub fn url_name(u: &str) -> Option<String> {
    let u = url::Url::parse(u).ok()?;
    let last = u.path_segments()?.rev().find(|s| !s.is_empty())?;
    Some(decode(last)).filter(|s| !s.is_empty())
}

/// 去除 Windows 非法字符
pub fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .take(200)
        .collect();
    let s = s.trim().trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() { "download".into() } else { s }
}
