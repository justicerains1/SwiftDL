//! Windows 系统能力：磁盘空间、回收站、下载来源标记（MOTW）、安全随机数、网络连通性通知、错误分类。
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// 测试用：非 0 时代替真实可用空间
#[cfg(test)]
pub static FAKE_FREE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 目录所在磁盘的可用空间（字节）
pub fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(test)]
    {
        let f = FAKE_FREE.load(Ordering::Relaxed);
        if f > 0 {
            return Some(f);
        }
    }
    // 目录可能尚未创建：向上找到存在的祖先目录
    let mut d = dir;
    while !d.exists() {
        d = d.parent()?;
    }
    let w = wide(d);
    let mut free = 0u64;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(w.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
    };
    (ok != 0).then_some(free)
}

/// 移到回收站；失败（如路径过长、网络盘）时返回 false
pub fn recycle(path: &Path) -> bool {
    use windows_sys::Win32::UI::Shell::*;
    if !path.exists() {
        return true;
    }
    // pFrom 需以两个 NUL 结尾
    let mut from = wide(path);
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    unsafe { SHFileOperationW(&mut op) == 0 && op.fAnyOperationsAborted == 0 && !path.exists() }
}

/// 删除文件（或多文件 BT 任务的文件夹）：优先移到回收站，不行再直接删除
pub fn delete_file(path: &Path, to_recycle: bool) {
    if !(to_recycle && recycle(path)) {
        let _ = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    }
}

/// 在指定偏移写入整块数据（不移动文件指针，多个连接可同时写同一个文件）
pub fn write_at(file: &std::fs::File, data: &[u8], off: u64) -> Result<(), String> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < data.len() {
        match file.seek_write(&data[done..], off + done as u64) {
            Ok(0) => return Err(tr!("写入 0 字节").into()),
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(trf!("写入失败: {}", e)),
        }
    }
    Ok(())
}

/// 去掉 URL 中的账号密码，避免写入文件属性或显示
pub fn strip_credentials(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        Err(_) => url.to_string(),
    }
}

/// 写入“来自 Internet”标记（Zone.Identifier 备用数据流），让 SmartScreen / Office 受保护视图生效
pub fn mark_from_web(path: &Path, host_url: &str, referrer: Option<&str>) -> bool {
    // 只标记已存在的普通文件：对不存在的路径写备用数据流会凭空创建一个空文件
    if !path.is_file() {
        return false;
    }
    let mut s = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(r) = referrer.filter(|r| r.starts_with("http")) {
        s.push_str(&format!("ReferrerUrl={}\r\n", strip_credentials(r)));
    }
    if host_url.contains("://") {
        s.push_str(&format!("HostUrl={}\r\n", strip_credentials(host_url)));
    }
    let mut ads = path.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    std::fs::write(ads, s).is_ok()
}

#[cfg(test)]
pub fn read_mark(path: &Path) -> Option<String> {
    let mut ads = path.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    std::fs::read_to_string(ads).ok()
}

/// 系统安全随机数生成的令牌（32 位十六进制）
pub fn random_token() -> String {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
    let mut b = [0u8; 16];
    let st = unsafe { BCryptGenRandom(std::ptr::null_mut(), b.as_mut_ptr(), b.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if st != 0 {
        // 极少见：退化为时间与地址混合
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        b = (t ^ (&b as *const _ as u128)).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ───────────── 网络连通性 ─────────────

/// 网络从“不可用”变为“可访问”时置位，由后台节拍消费
pub static NET_RESTORED: AtomicBool = AtomicBool::new(false);
static NET_LEVEL: AtomicI32 = AtomicI32::new(-1);

unsafe extern "system" fn on_hint(_ctx: *const core::ffi::c_void, hint: windows_sys::Win32::Networking::WinSock::NL_NETWORK_CONNECTIVITY_HINT) {
    use windows_sys::Win32::Networking::WinSock::*;
    let lvl = hint.ConnectivityLevel;
    let up = |l: i32| l == NetworkConnectivityLevelHintInternetAccess || l == NetworkConnectivityLevelHintConstrainedInternetAccess || l == NetworkConnectivityLevelHintLocalAccess;
    let prev = NET_LEVEL.swap(lvl, Ordering::Relaxed);
    if up(lvl) && prev != -1 && !up(prev) {
        NET_RESTORED.store(true, Ordering::Relaxed);
    }
}

/// 注册系统网络连通性变化通知（Windows 10 2004+）
pub fn watch_network() {
    let mut h: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
    unsafe {
        windows_sys::Win32::NetworkManagement::IpHelper::NotifyNetworkConnectivityHintChange(Some(on_hint), std::ptr::null(), true, &mut h);
    }
}

/// 后台节拍间隔远大于预期时，说明系统刚从睡眠/休眠中唤醒
pub fn woke_from_sleep(gap_secs: u64) -> bool {
    gap_secs >= 20
}

/// 证书校验失败（不受信任的根、自签名、过期、域名不符、吊销等）。
/// schannel 的错误文字随系统语言变化，故按 “os error <HRESULT>” 中的错误码判断。
pub fn is_cert_code_error(e: &str) -> bool {
    const CODES: [u32; 6] = [
        0x8009_0322, // SEC_E_WRONG_PRINCIPAL
        0x8009_0325, // SEC_E_UNTRUSTED_ROOT
        0x8009_0327, // SEC_E_CERT_UNKNOWN
        0x8009_0328, // SEC_E_CERT_EXPIRED
        0x8009_0349, // SEC_E_CERT_WRONG_USAGE
        0x8009_6004, // TRUST_E_CERT_SIGNATURE
    ];
    e.match_indices("os error ").any(|(i, k)| {
        let rest = &e[i + k.len()..];
        let end = rest.find(|c: char| c != '-' && !c.is_ascii_digit()).unwrap_or(rest.len());
        let Ok(n) = rest[..end].parse::<i64>() else { return false };
        let c = n as u32;
        // CERT_E_*（0x800B01xx）、CRYPT_E_* 吊销检查（0x800920xx）、TRUST_E_* 证书链（0x800960xx）
        c & 0xFFFF_FF00 == 0x800B_0100 || c & 0xFFFF_FF00 == 0x8009_2000 || c & 0xFFFF_FF00 == 0x8009_6000 || CODES.contains(&c)
    })
}

/// 重定向错误中的标记（两种语言的提示都以它结尾，便于识别目标主机）
pub const REDIRECT_MARK: &str = "(redirect: ";

/// 从“信任站点被重定向到未信任的 HTTPS 站点”的错误中取出目标主机
pub fn redirect_host(e: &str) -> Option<&str> {
    let rest = &e[e.rfind(REDIRECT_MARK)? + REDIRECT_MARK.len()..];
    Some(&rest[..rest.find(')')?]).filter(|h| !h.is_empty())
}

/// 需要用户决定是否信任站点的错误：证书不可信，或被重定向到未经证书校验的站点。
/// 这类错误重试无意义，不自动重试。
pub fn is_cert_error(e: &str) -> bool {
    is_cert_code_error(e) || redirect_host(e).is_some()
}

/// 出错后点“信任此站点”应信任的主机：重定向错误取跳转目标，证书错误取链接所在主机
pub fn trust_target(e: &str, url: &str) -> Option<String> {
    if let Some(h) = redirect_host(e) {
        return Some(h.to_ascii_lowercase());
    }
    if is_cert_code_error(e) { super::host_key(url) } else { None }
}

/// 错误分类：网络类错误在网络恢复后自动重试，且不消耗自动重试次数
pub fn is_network_error(e: &str) -> bool {
    if is_cert_error(e) {
        return false; // 证书问题重试无意义，需用户决定是否信任
    }
    let l = e.to_ascii_lowercase();
    [
        "error sending request",
        "connection",
        "connect",
        "timed out",
        "dns",
        "network",
        "os error 10051",
        "os error 10060",
        "os error 10065",
        "os error 11001",
        "unexpected eof",
        "error decoding response body",
        "连接停滞",
        "连接提前断开",
    ]
    .iter()
    .any(|k| l.contains(k))
        && !l.contains("http 4")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        let tmp = std::env::temp_dir().join(format!("swiftdl-sys-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(free_space(&tmp.join("not/yet/created")).unwrap() > 0);

        let f = tmp.join("a.bin");
        std::fs::write(&f, b"hello").unwrap();
        assert!(mark_from_web(&f, "https://user:pw@cdn.com/a.bin", Some("https://site.com/page")));
        let z = read_mark(&f).unwrap();
        assert!(z.contains("ZoneId=3") && z.contains("HostUrl=https://cdn.com/a.bin") && z.contains("ReferrerUrl=https://site.com/page"));
        assert!(!z.contains("pw"), "不应写入密码");
        assert_eq!(std::fs::read(&f).unwrap(), b"hello", "备用数据流不影响文件内容");

        delete_file(&f, true);
        assert!(!f.exists());

        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);

        assert!(is_network_error("error sending request for url (https://a.com/): tcp connect error"));
        assert!(is_network_error("连接停滞，正在重连"));
        assert!(!is_network_error("HTTP 404"));
        assert!(!is_network_error("磁盘空间不足"));
        let cert = "error sending request for url (https://a.com/): client error (Connect): 证书链由不受信任的颁发机构颁发。 (os error -2146762487)";
        assert!(is_cert_error(cert) && !is_network_error(cert));
        assert!(is_cert_error("The certificate has expired (os error -2146762495)"));
        assert!(is_cert_error("SEC_E_WRONG_PRINCIPAL (os error -2146893022)"));
        assert!(!is_cert_error("tcp connect error: (os error 10060)"));
        assert!(!is_cert_error("HTTP 404"));
        assert_eq!(trust_target(cert, "https://Self.Signed.com/a.zip").as_deref(), Some("self.signed.com"));
        let redir = "error following redirect for url (https://cdn.x.com/a): 下载被重定向到未信任的站点 (redirect: cdn.x.com)";
        assert!(is_cert_error(redir) && !is_cert_code_error(redir) && !is_network_error(redir));
        assert_eq!(trust_target(redir, "https://intranet.local/a").as_deref(), Some("cdn.x.com"));
        assert_eq!(trust_target("HTTP 404", "https://a.com/"), None);
        assert!(woke_from_sleep(60) && !woke_from_sleep(1));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
