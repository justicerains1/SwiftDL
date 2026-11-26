//! 本地服务：浏览器扩展与第二实例通过它投递下载任务。
use crate::engine::{AddReq, Manager, UiEvent};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use std::sync::Arc;
use std::sync::mpsc::Sender;

#[derive(Clone)]
struct St {
    tx: Sender<UiEvent>,
    m: Arc<Manager>,
}

pub async fn serve(listener: tokio::net::TcpListener, tx: Sender<UiEvent>, m: Arc<Manager>) {
    let app = axum::Router::new()
        .route("/ping", get(|| async { "swiftdl" }))
        .route("/add", post(add))
        .route("/show", post(show))
        .route("/hello", post(hello))
        .with_state(St { tx, m });
    let _ = axum::serve(listener, app).await;
}

pub const TOKEN_HEADER: &str = "x-swiftdl-token";

/// 只接受扩展或本地进程（无 Origin）的请求，且必须携带配对令牌：
/// 拒绝普通网页，也拒绝同机其他用户（多人远程桌面）投递任务
fn allowed(s: &St, h: &HeaderMap) -> bool {
    let origin_ok = match h.get("origin").and_then(|v| v.to_str().ok()) {
        None => true,
        Some(o) => o.starts_with("chrome-extension://") || o.starts_with("moz-extension://"),
    };
    let token = s.m.settings.read().unwrap().token.clone();
    let given = h.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
    origin_ok && !token.is_empty() && ct_eq(given.as_bytes(), token.as_bytes())
}

/// 常量时间比较，避免按响应时间猜测令牌
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn add(State(s): State<St>, h: HeaderMap, Json(req): Json<AddReq>) -> StatusCode {
    if !allowed(&s, &h) {
        return StatusCode::FORBIDDEN;
    }
    let l = req.url.to_ascii_lowercase();
    if !["http://", "https://", "ftp://", "ftps://", "sftp://", "magnet:?"].iter().any(|p| l.starts_with(p)) && !crate::engine::bt::is_bt(&req.url) {
        return StatusCode::BAD_REQUEST;
    }
    let _ = s.tx.send(UiEvent::Add(vec![req]));
    s.m.repaint();
    StatusCode::OK
}

async fn show(State(s): State<St>, h: HeaderMap) -> StatusCode {
    if !allowed(&s, &h) {
        return StatusCode::FORBIDDEN;
    }
    let _ = s.tx.send(UiEvent::Show);
    s.m.repaint();
    StatusCode::OK
}

pub const EXT_VERSION_HEADER: &str = "x-swiftdl-ext-version";

/// 扩展启动/安装时报到，用于确认扩展已连接；双方交换版本号，扩展据此在程序升级后重新加载自身
async fn hello(State(s): State<St>, h: HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !allowed(&s, &h) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(v) = h.get(EXT_VERSION_HEADER).and_then(|v| v.to_str().ok()) {
        let v: String = v.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.').take(32).collect();
        if *crate::browser::EXT_VERSION.lock().unwrap() != v {
            log_info!("browser extension v{v} connected (app v{})", env!("CARGO_PKG_VERSION"));
        }
        *crate::browser::EXT_VERSION.lock().unwrap() = v;
    }
    crate::browser::LAST_HELLO.store(crate::engine::now(), std::sync::atomic::Ordering::Relaxed);
    s.m.repaint();
    Json(serde_json::json!({ "app": env!("CARGO_PKG_VERSION") })).into_response()
}

/// 已有实例在运行：把参数转交给它。返回 true 表示已转交。
pub async fn forward(port: u16, token: &str, urls: &[String]) -> bool {
    // 必须有超时：占用端口的若是别的程序且不应答，启动会一直卡在这里
    let c = reqwest::Client::builder().no_proxy().timeout(std::time::Duration::from_secs(3)).build().unwrap_or_default();
    let base = format!("http://127.0.0.1:{port}");
    let ok = c.get(format!("{base}/ping")).send().await.ok().filter(|r| r.status().is_success());
    if ok.is_none() {
        return false;
    }
    if urls.is_empty() {
        let _ = c.post(format!("{base}/show")).header(TOKEN_HEADER, token).send().await;
    }
    for u in urls {
        let body = serde_json::to_vec(&AddReq { url: u.clone(), ..Default::default() }).unwrap_or_default();
        let _ = c.post(format!("{base}/add")).header("content-type", "application/json").header(TOKEN_HEADER, token).body(body).send().await;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_and_origin() {
        let root = std::env::temp_dir().join(format!("swiftdl-server-{}", std::process::id()));
        unsafe { std::env::set_var("APPDATA", root.join("appdata")) };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let m = Manager::new(rt.handle().clone(), Default::default());
        let token = m.settings.read().unwrap().token.clone();
        assert_eq!(token.len(), 32, "首次启动应生成令牌");
        let (tx, rx) = std::sync::mpsc::channel();
        let l = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        rt.spawn(serve(l, tx, m.clone()));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        let body = r#"{"url":"https://a.com/x.zip"}"#;
        let post = |path: &str, tok: Option<&str>, origin: Option<&str>| {
            let mut rb = c.post(format!("{base}{path}")).header("content-type", "application/json").body(body);
            if let Some(t) = tok {
                rb = rb.header(TOKEN_HEADER, t);
            }
            if let Some(o) = origin {
                rb = rb.header("origin", o);
            }
            rt.block_on(rb.send()).unwrap().status().as_u16()
        };
        assert_eq!(rt.block_on(c.get(format!("{base}/ping")).send()).unwrap().status().as_u16(), 200, "ping 无需令牌");
        assert_eq!(post("/add", None, None), 403, "无令牌");
        assert_eq!(post("/add", Some("wrong"), None), 403, "错误令牌");
        assert_eq!(post("/add", Some(&token), Some("https://evil.com")), 403, "网页来源即使有令牌也拒绝");
        assert_eq!(post("/add", Some(&token), Some("chrome-extension://abc")), 200);
        assert_eq!(post("/add", Some(&token), None), 200, "本机进程携带令牌");
        assert_eq!(post("/hello", None, Some("chrome-extension://abc")), 403);
        assert_eq!(post("/hello", Some(&token), Some("chrome-extension://abc")), 200);
        assert_eq!(rx.try_iter().filter(|e| matches!(e, UiEvent::Add(_))).count(), 2);

        // 握手交换版本：程序返回自身版本，记录扩展版本；不带版本头（弹窗查询）时不覆盖
        let hello = |ver: Option<&str>| {
            let mut rb = c.post(format!("{base}/hello")).header(TOKEN_HEADER, &token).header("origin", "chrome-extension://abc");
            if let Some(v) = ver {
                rb = rb.header(EXT_VERSION_HEADER, v);
            }
            rt.block_on(async { rb.send().await.unwrap().json::<serde_json::Value>().await.unwrap() })
        };
        assert_eq!(hello(Some("0.4.0<script>"))["app"], env!("CARGO_PKG_VERSION"));
        assert_eq!(*crate::browser::EXT_VERSION.lock().unwrap(), "0.4.0script", "只保留安全字符");
        assert_eq!(crate::browser::ext_outdated().as_deref(), Some("0.4.0script"));
        hello(None);
        assert_eq!(*crate::browser::EXT_VERSION.lock().unwrap(), "0.4.0script");
        hello(Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(crate::browser::ext_outdated(), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
