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
        .with_state(St { tx, m });
    let _ = axum::serve(listener, app).await;
}

/// 只接受扩展或本地进程（无 Origin）的请求，拒绝普通网页
fn allowed(h: &HeaderMap) -> bool {
    match h.get("origin").and_then(|v| v.to_str().ok()) {
        None => true,
        Some(o) => o.starts_with("chrome-extension://") || o.starts_with("moz-extension://"),
    }
}

async fn add(State(s): State<St>, h: HeaderMap, Json(req): Json<AddReq>) -> StatusCode {
    if !allowed(&h) {
        return StatusCode::FORBIDDEN;
    }
    if !(req.url.starts_with("http://") || req.url.starts_with("https://")) {
        return StatusCode::BAD_REQUEST;
    }
    let _ = s.tx.send(UiEvent::Add(vec![req]));
    s.m.repaint();
    StatusCode::OK
}

async fn show(State(s): State<St>, h: HeaderMap) -> StatusCode {
    if !allowed(&h) {
        return StatusCode::FORBIDDEN;
    }
    let _ = s.tx.send(UiEvent::Show);
    s.m.repaint();
    StatusCode::OK
}

/// 已有实例在运行：把参数转交给它。返回 true 表示已转交。
pub async fn forward(port: u16, urls: &[String]) -> bool {
    let c = reqwest::Client::builder().no_proxy().build().unwrap_or_default();
    let base = format!("http://127.0.0.1:{port}");
    let ok = c.get(format!("{base}/ping")).send().await.ok().filter(|r| r.status().is_success());
    if ok.is_none() {
        return false;
    }
    if urls.is_empty() {
        let _ = c.post(format!("{base}/show")).send().await;
    }
    for u in urls {
        let body = serde_json::to_vec(&AddReq { url: u.clone(), ..Default::default() }).unwrap_or_default();
        let _ = c.post(format!("{base}/add")).header("content-type", "application/json").body(body).send().await;
    }
    true
}
