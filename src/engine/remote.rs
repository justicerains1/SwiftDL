//! FTP / FTPS / SFTP 下载源：探测大小与续传能力，按偏移打开字节流，交给通用分段引擎。
//! 密码不保存在链接里：用 Windows DPAPI 加密后存入任务；SFTP 主机密钥首次信任并记录，变化时拒绝连接。
use super::probe::Info;
use super::{AddReq, store};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// 远程源的错误分类：与 HTTP 一样区分网络错误、不可恢复错误、服务器限流
#[derive(Debug)]
pub enum Fail {
    Net(String),
    Fatal(String),
    Throttle(String),
}

impl Fail {
    pub fn msg(&self) -> &str {
        match self {
            Fail::Net(m) | Fail::Fatal(m) | Fail::Throttle(m) => m,
        }
    }
}

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, Fail>> + Send>>;

pub fn is_remote(url: &str) -> bool {
    let l = url.trim_start().to_ascii_lowercase();
    l.starts_with("ftp://") || l.starts_with("ftps://") || l.starts_with("sftp://")
}

// ───────────── 凭据（DPAPI） ─────────────

fn blob(data: &[u8]) -> windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
    windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
}

fn dpapi(data: &[u8], protect: bool) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::*;
    let input = blob(data);
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        if protect {
            CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        }
    };
    if ok == 0 {
        return None;
    }
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
    unsafe { windows_sys::Win32::Foundation::LocalFree(out.pbData as _) };
    Some(v)
}

/// 用当前 Windows 用户的 DPAPI 加密（十六进制），其他用户或其他电脑无法解密
pub fn protect(s: &str) -> Option<String> {
    dpapi(s.as_bytes(), true).map(|v| v.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn unprotect(h: &str) -> Option<String> {
    let bytes: Option<Vec<u8>> = (0..h.len()).step_by(2).map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect();
    dpapi(&bytes?, false).and_then(|v| String::from_utf8(v).ok())
}

/// 新建任务时：把链接中的密码移出并加密保存
pub fn secure_req(req: &mut AddReq) {
    if !is_remote(&req.url) {
        return;
    }
    if let Ok(mut u) = url::Url::parse(&req.url)
        && let Some(pw) = u.password().map(|p| percent_encoding::percent_decode_str(p).decode_utf8_lossy().into_owned())
        && let Some(sec) = protect(&pw)
    {
        req.secret = Some(sec);
        let _ = u.set_password(None);
        req.url = u.to_string();
    }
}

pub struct Creds {
    pub user: String,
    pub pass: Option<String>,
    pub key: Option<PathBuf>,
}

pub fn creds(req: &AddReq, u: &url::Url) -> Creds {
    let dec = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
    let user = if u.username().is_empty() {
        if u.scheme() == "sftp" { std::env::var("USERNAME").unwrap_or_else(|_| "root".into()) } else { "anonymous".into() }
    } else {
        dec(u.username())
    };
    let pass = req.secret.as_deref().and_then(unprotect).or_else(|| u.password().map(dec));
    Creds { user, pass, key: req.key_path.clone().filter(|k| !k.trim().is_empty()).map(PathBuf::from) }
}

fn remote_path(u: &url::Url) -> String {
    let p = percent_encoding::percent_decode_str(u.path()).decode_utf8_lossy().into_owned();
    // sftp://host/~/file 表示用户主目录下的相对路径
    p.strip_prefix("/~/").map(str::to_string).unwrap_or(p)
}

fn file_name(u: &url::Url) -> String {
    super::probe::url_name(u.as_str()).map(|n| super::probe::sanitize(&n)).unwrap_or_else(|| "download".into())
}

// ───────────── FTP / FTPS ─────────────

use suppaftp::tokio::{AsyncFtpStream, AsyncNativeTlsConnector, AsyncNativeTlsFtpStream, ImplAsyncFtpStream};
use suppaftp::types::FileType;

enum Ftp {
    Plain(AsyncFtpStream),
    Tls(AsyncNativeTlsFtpStream),
}

macro_rules! ftp_do {
    ($c:expr, $s:ident => $e:expr) => {
        match $c {
            Ftp::Plain($s) => $e,
            Ftp::Tls($s) => $e,
        }
    };
}

fn ftp_fail(e: suppaftp::FtpError) -> Fail {
    let s = e.to_string();
    let code = match &e {
        suppaftp::FtpError::UnexpectedResponse(r) => Some(r.status.code()),
        _ => None,
    };
    match code {
        Some(421) => Fail::Throttle(trf!("FTP 服务器连接数已满 ({})", s)),
        Some(530) => Fail::Fatal(tr!("FTP 登录失败：用户名或密码错误").into()),
        Some(550) => Fail::Fatal(trf!("文件不存在或无权访问 ({})", s)),
        _ => Fail::Net(s),
    }
}

async fn ftp_connect(u: &url::Url, c: &Creds) -> Result<Ftp, Fail> {
    let host = u.host_str().ok_or_else(|| Fail::Fatal(tr!("链接缺少主机名").into()))?;
    let port = u.port().unwrap_or(21);
    let mut conn = if u.scheme() == "ftps" {
        let s = ImplAsyncFtpStream::connect((host, port)).await.map_err(ftp_fail)?;
        let tls = AsyncNativeTlsConnector::from(async_native_tls::TlsConnector::new());
        Ftp::Tls(s.into_secure(tls, host).await.map_err(ftp_fail)?)
    } else {
        Ftp::Plain(AsyncFtpStream::connect((host, port)).await.map_err(ftp_fail)?)
    };
    let pass = c.pass.clone().unwrap_or_else(|| "anonymous@".into());
    ftp_do!(&mut conn, s => s.login(c.user.as_str(), pass.as_str()).await).map_err(ftp_fail)?;
    ftp_do!(&mut conn, s => s.transfer_type(FileType::Binary).await).map_err(ftp_fail)?;
    Ok(conn)
}

async fn ftp_probe(req: &AddReq, u: &url::Url) -> Result<Info, String> {
    let c = creds(req, u);
    let mut conn = ftp_connect(u, &c).await.map_err(|e| e.msg().to_string())?;
    let path = remote_path(u);
    let size = ftp_do!(&mut conn, s => s.size(&path).await).map_err(|e| ftp_fail(e).msg().to_string())? as u64;
    // REST 被接受即支持断点续传与多连接
    let resumable = ftp_do!(&mut conn, s => s.resume_transfer(0).await).is_ok();
    let _ = ftp_do!(&mut conn, s => s.quit().await);
    Ok(Info { size: Some(size), resumable, filename: file_name(u), validator: None, hls: None })
}

/// 读取流与保持其所需连接的组合
struct Holder<C, R> {
    _conn: C,
    reader: Pin<Box<R>>,
}

impl<C: Send + Unpin, R: AsyncRead + Send> AsyncRead for Holder<C, R> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        self.reader.as_mut().poll_read(cx, buf)
    }
}

fn into_stream<R: AsyncRead + Send + 'static>(r: R) -> ByteStream {
    Box::pin(tokio_util::io::ReaderStream::with_capacity(r, 256 << 10).map(|r| r.map_err(|e| Fail::Net(e.to_string()))))
}

async fn ftp_open(req: &AddReq, u: &url::Url, pos: u64) -> Result<ByteStream, Fail> {
    let c = creds(req, u);
    let mut conn = ftp_connect(u, &c).await?;
    let path = remote_path(u);
    if pos > 0 {
        ftp_do!(&mut conn, s => s.resume_transfer(pos as usize).await).map_err(ftp_fail)?;
    }
    match conn {
        Ftp::Plain(mut s) => {
            let t = s.retr_as_stream(&path).await.map_err(ftp_fail)?;
            Ok(into_stream(Holder { _conn: s, reader: Box::pin(t) }))
        }
        Ftp::Tls(mut s) => {
            let t = s.retr_as_stream(&path).await.map_err(ftp_fail)?;
            Ok(into_stream(Holder { _conn: s, reader: Box::pin(t) }))
        }
    }
}

// ───────────── SFTP ─────────────

/// 主机密钥首次信任：记录 host:port 的指纹，之后必须一致
pub fn verify_host_key(host_port: &str, fingerprint: &str) -> Result<(), String> {
    let path = store::data_dir().join("known_hosts");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    for line in text.lines() {
        if let Some((h, fp)) = line.split_once(' ')
            && h == host_port
        {
            return if fp == fingerprint {
                Ok(())
            } else {
                Err(trf!("{} 的主机密钥已变化（可能遭到中间人攻击），已拒绝连接。若服务器确实更换了密钥，请删除 {} 中对应的行", host_port, path.display()))
            };
        }
    }
    let _ = std::fs::create_dir_all(store::data_dir());
    let mut t = text;
    t.push_str(&format!("{host_port} {fingerprint}\n"));
    std::fs::write(&path, t).map_err(|e| e.to_string())
}

struct SshClient {
    host_port: String,
    err: Arc<Mutex<Option<String>>>,
}

impl russh::client::Handler for SshClient {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        use russh::keys::{HashAlg, PublicKeyOrCertificate};
        let fp = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256).to_string(),
            PublicKeyOrCertificate::Certificate(c) => c.public_key().fingerprint(HashAlg::Sha256).to_string(),
        };
        match verify_host_key(&self.host_port, &fp) {
            Ok(()) => Ok(true),
            Err(e) => {
                *self.err.lock().unwrap() = Some(e);
                Ok(false)
            }
        }
    }
}

struct Sftp {
    _ssh: russh::client::Handle<SshClient>,
    session: russh_sftp::client::SftpSession,
}

async fn sftp_connect(u: &url::Url, c: &Creds) -> Result<Sftp, Fail> {
    let host = u.host_str().ok_or_else(|| Fail::Fatal(tr!("链接缺少主机名").into()))?;
    let port = u.port().unwrap_or(22);
    let err = Arc::new(Mutex::new(None));
    let handler = SshClient { host_port: format!("{host}:{port}"), err: err.clone() };
    let cfg = Arc::new(russh::client::Config { inactivity_timeout: Some(std::time::Duration::from_secs(60)), ..Default::default() });
    let mut ssh = match russh::client::connect(cfg, (host, port), handler).await {
        Ok(h) => h,
        Err(e) => {
            let host_err = err.lock().unwrap().take();
            return Err(match host_err {
                Some(m) => Fail::Fatal(m),
                None => Fail::Net(e.to_string()),
            });
        }
    };
    let ok = if let Some(pw) = &c.pass {
        ssh.authenticate_password(c.user.clone(), pw.clone()).await.map_err(|e| Fail::Net(e.to_string()))?.success()
    } else {
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
        let candidates: Vec<PathBuf> = match &c.key {
            Some(k) => vec![k.clone()],
            None => ["id_ed25519", "id_ecdsa", "id_rsa"].iter().map(|n| home.join(".ssh").join(n)).filter(|p| p.exists()).collect(),
        };
        let mut ok = false;
        for k in candidates {
            let Ok(key) = russh::keys::load_secret_key(&k, None) else { continue };
            let hash = ssh.best_supported_rsa_hash().await.ok().flatten().flatten();
            let key = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            if ssh.authenticate_publickey(c.user.clone(), key).await.map(|r| r.success()).unwrap_or(false) {
                ok = true;
                break;
            }
        }
        ok
    };
    if !ok {
        return Err(Fail::Fatal(tr!("SFTP 登录失败：用户名、密码或密钥不正确").into()));
    }
    let ch = ssh.channel_open_session().await.map_err(|e| Fail::Net(e.to_string()))?;
    ch.request_subsystem(true, "sftp").await.map_err(|e| Fail::Net(e.to_string()))?;
    let session = russh_sftp::client::SftpSession::new(ch.into_stream()).await.map_err(|e| Fail::Net(e.to_string()))?;
    Ok(Sftp { _ssh: ssh, session })
}

fn sftp_fail(e: russh_sftp::client::error::Error) -> Fail {
    let s = e.to_string();
    if s.contains("No such file") || s.contains("NoSuchFile") || s.contains("Permission") {
        Fail::Fatal(trf!("文件不存在或无权访问 ({})", s))
    } else {
        Fail::Net(s)
    }
}

async fn sftp_probe(req: &AddReq, u: &url::Url) -> Result<Info, String> {
    let c = creds(req, u);
    let s = sftp_connect(u, &c).await.map_err(|e| e.msg().to_string())?;
    let md = s.session.metadata(remote_path(u)).await.map_err(|e| sftp_fail(e).msg().to_string())?;
    let validator = md.mtime.map(|t| t.to_string());
    Ok(Info { size: md.size, resumable: md.size.is_some(), filename: file_name(u), validator, hls: None })
}

async fn sftp_open(req: &AddReq, u: &url::Url, pos: u64) -> Result<ByteStream, Fail> {
    use tokio::io::AsyncSeekExt;
    let c = creds(req, u);
    let s = sftp_connect(u, &c).await?;
    let mut f = s.session.open(remote_path(u)).await.map_err(sftp_fail)?;
    if pos > 0 {
        f.seek(std::io::SeekFrom::Start(pos)).await.map_err(|e| Fail::Net(e.to_string()))?;
    }
    Ok(into_stream(Holder { _conn: s, reader: Box::pin(f) }))
}

impl Unpin for Sftp {}

// ───────────── 对外接口 ─────────────

fn parse(url: &str) -> Result<url::Url, Fail> {
    url::Url::parse(url).map_err(|e| Fail::Fatal(trf!("无效的链接：{}", e)))
}

pub async fn probe(req: &AddReq) -> Result<Info, String> {
    let u = parse(&req.url).map_err(|e| e.msg().to_string())?;
    match u.scheme() {
        "sftp" => sftp_probe(req, &u).await,
        _ => ftp_probe(req, &u).await,
    }
}

/// 从偏移 pos 开始读取到文件末尾的字节流（调用方读到分段结尾后丢弃即可）
pub async fn open(req: &AddReq, pos: u64) -> Result<ByteStream, Fail> {
    let u = parse(&req.url)?;
    match u.scheme() {
        "sftp" => sftp_open(req, &u, pos).await,
        _ => ftp_open(req, &u, pos).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_and_secure_req() {
        let h = protect("p@ss 密码").unwrap();
        assert!(!h.contains("p@ss"));
        assert_eq!(unprotect(&h).as_deref(), Some("p@ss 密码"));
        assert_eq!(unprotect("zz"), None);

        let mut r = AddReq { url: "ftp://alice:s%40cret@ftp.example.com/pub/a.iso".into(), ..Default::default() };
        secure_req(&mut r);
        assert_eq!(r.url, "ftp://alice@ftp.example.com/pub/a.iso", "链接中不再包含密码");
        let u = url::Url::parse(&r.url).unwrap();
        let c = creds(&r, &u);
        assert_eq!((c.user.as_str(), c.pass.as_deref()), ("alice", Some("s@cret")));

        let anon = AddReq { url: "ftp://h/x".into(), ..Default::default() };
        assert_eq!(creds(&anon, &url::Url::parse(&anon.url).unwrap()).user, "anonymous");
        assert_eq!(remote_path(&url::Url::parse("sftp://h/~/dir/a%20b.bin").unwrap()), "dir/a b.bin");
    }

    #[test]
    fn host_key_tofu() {
        let root = std::env::temp_dir().join(format!("swiftdl-kh-{}", std::process::id()));
        unsafe { std::env::set_var("APPDATA", root.join("appdata")) };
        assert!(verify_host_key("h:22", "SHA256:aaa").is_ok(), "首次连接应信任并记录");
        assert!(verify_host_key("h:22", "SHA256:aaa").is_ok());
        let e = verify_host_key("h:22", "SHA256:bbb").unwrap_err();
        assert!(e.contains("主机密钥已变化"), "{e}");
        assert!(verify_host_key("h:2222", "SHA256:bbb").is_ok(), "不同端口独立记录");
        let _ = std::fs::remove_dir_all(&root);
    }
}
