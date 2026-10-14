//! 传输层：pkt-line 编码 + 两种远端（本地路径 / smart HTTP v1）。
//!
//! smart HTTP 流程（v1）：
//!   GET  {url}/info/refs?service=git-upload-pack|git-receive-pack  → ref 广告
//!   POST {url}/git-upload-pack|git-receive-pack                    → 数据交换
//! 请求体 / 响应体都是「pkt-line 行 + 二进制 pack」的混合流。
//!
//! HTTP 借道 curl 子进程：TLS / 分块编码 / 服务端断连的处理久经验证，
//! 且 curl 几乎无处不在（认证用 -u，即 URL 凭据或 MINI_GIT_USER/PASS）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::objects::{parse_commit, parse_tree, read_object, Obj, ObjKind, Oid};
use crate::pack;
use crate::repo::Repo;

// ---- pkt-line ----

pub fn pkt_line(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    out
}

pub const FLUSH_PKT: &[u8] = b"0000";

/// 从混合流头部剥离 pkt-line（ref 广告 / ACK 段），返回剩余的原始 pack 字节。
fn strip_pkt_lines(mut buf: &[u8]) -> &[u8] {
    loop {
        if buf.len() < 4 || &buf[..4] == b"PACK" {
            return buf; // pack 数据开始
        }
        let Ok(len) = usize::from_str_radix(std::str::from_utf8(&buf[..4]).unwrap_or(""), 16)
        else {
            return buf;
        };
        if len == 0 {
            buf = &buf[4..]; // flush-pkt
            continue;
        }
        if len > buf.len() {
            return &buf[4..]; // 异常容错：按 pack 开始处理
        }
        buf = &buf[len..]; // ref 行 / ACK / NAK / unpack 结果等
    }
}

// ---- 远端抽象 ----

pub struct RemoteRef {
    pub name: String,
    pub oid: Oid,
}

pub enum Transport {
    /// 本地路径（bare 或普通仓库）：直接读写对象与 ref。
    Local(PathBuf),
    /// smart HTTP(S)。
    Http {
        base: String,
        user: Option<String>,
        pass: Option<String>,
    },
    /// SSH（借道 ssh 命令；认证、密钥、别名全部走用户的 ~/.ssh 配置）。
    Ssh {
        user: Option<String>,
        host: String,
        port: Option<u16>,
        path: String,
    },
}

impl Transport {
    /// 解析远端地址：http(s):// 走 HTTP；ssh:// 与 scp 风格（user@host:path）
    /// 走 SSH；否则视为本地路径。
    pub fn parse(url: &str) -> Result<Self, String> {
        if url.starts_with("http://") || url.starts_with("https://") {
            let mut base = url.trim_end_matches('/').to_string();
            // URL 内嵌认证（https://user:pass@host/...）→ 提取给 curl
            let (user, pass) = if let Some(at) = base.find('@') {
                let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
                if at > scheme_end {
                    let creds = base[scheme_end..at].to_string();
                    let (u, p) = creds.split_once(':').unwrap_or((creds.as_str(), ""));
                    base = format!("{}{}", &base[..scheme_end], &base[at + 1..]);
                    (Some(u.to_string()), Some(p.to_string()))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };
            let user = user.or_else(|| std::env::var("MINI_GIT_USER").ok());
            let pass = pass.or_else(|| std::env::var("MINI_GIT_PASS").ok());
            Ok(Self::Http { base, user, pass })
        } else if let Some(rest) = url.strip_prefix("ssh://") {
            // ssh://[user@]host[:port]/仓库路径
            let (authority, path) = rest
                .split_once('/')
                .ok_or_else(|| format!("ssh 地址缺少路径：{url}"))?;
            Ok(Self::Ssh {
                user: authority.split_once('@').map(|(u, _)| u.to_string()),
                host: authority
                    .rsplit_once('@')
                    .map(|(_, h)| h.to_string())
                    .unwrap_or_else(|| authority.to_string()),
                port: None,
                path: path.trim_end_matches('/').to_string(),
            })
        } else if is_scp_style(url) {
            // git@github.com:owner/repo.git
            let (authority, path) = url
                .split_once(':')
                .ok_or_else(|| format!("scp 风格地址缺少路径：{url}"))?;
            Ok(Self::Ssh {
                user: authority.split_once('@').map(|(u, _)| u.to_string()),
                host: authority
                    .rsplit_once('@')
                    .map(|(_, h)| h.to_string())
                    .unwrap_or_else(|| authority.to_string()),
                port: None,
                path: path.trim_end_matches('/').to_string(),
            })
        } else {
            let path = PathBuf::from(shellexpand(url));
            if !path.exists() {
                return Err(format!(
                    "本地远端不存在：{}（若是 SSH 地址，请用 user@host:path 或 ssh:// 完整格式）",
                    path.display()
                ));
            }
            Ok(Self::Local(path))
        }
    }

    /// ref 发现。返回 (远端 ref 列表, 能力字符串)。
    pub fn ref_discovery(&self, service: &str) -> Result<(Vec<RemoteRef>, String), String> {
        match self {
            Self::Local(path) => {
                let repo = Repo {
                    workdir: path.clone(),
                    gitdir: gitdir_of(path)?,
                };
                let refs = repo
                    .list_refs()
                    .into_iter()
                    .map(|(name, oid)| RemoteRef { name, oid })
                    .collect();
                // 本地远端的默认分支：读其 HEAD 指向
                let head_target = std::fs::read_to_string(repo.gitdir.join("HEAD"))
                    .ok()
                    .and_then(|h| h.strip_prefix("ref: ").map(|s| s.trim().to_string()))
                    .unwrap_or_else(|| "refs/heads/main".into());
                Ok((
                    refs,
                    format!("symref=HEAD:{head_target} agent=mini-git/0.1"),
                ))
            }
            Self::Http { base, user, pass } => {
                let url = format!("{base}/info/refs?service=git-{service}");
                let body = http_bytes("GET", &url, &[], None, user.as_deref(), pass.as_deref())?;
                parse_ref_advertisement(&body)
            }
            Self::Ssh {
                user,
                host,
                port,
                path,
            } => {
                // SSH 会话：服务端直接发 ref 广告（无 "# service" 头），读至 flush-pkt
                let mut child = ssh_spawn(user, host, *port, service, path)?;
                let mut stdout = child.stdout.take().expect("stdout 已管道化");
                let ad = read_until_flush(&mut stdout)
                    .map_err(|e| format!("读取 SSH ref 广告失败: {e}"))?;
                let _ = child.wait();
                parse_ref_advertisement(&ad)
            }
        }
    }

    /// fetch：发送 want/have，取回 pack 字节。
    pub fn fetch_pack(&self, wants: &[Oid], haves: &[Oid]) -> Result<Vec<u8>, String> {
        match self {
            Self::Local(path) => {
                // 本地远端：无协商，把 want 可达对象重新打成无 delta pack
                let repo = Repo {
                    workdir: path.clone(),
                    gitdir: gitdir_of(path)?,
                };
                let objects = collect_reachable(&repo, wants, &[])?;
                Ok(pack::write_pack(&objects))
            }
            Self::Http { base, user, pass } => {
                let mut body = Vec::new();
                for (i, w) in wants.iter().enumerate() {
                    let caps = if i == 0 { " agent=mini-git/0.1" } else { "" };
                    body.extend_from_slice(&pkt_line(
                        format!("want {}{caps}\n", w.to_hex()).as_bytes(),
                    ));
                }
                body.extend_from_slice(FLUSH_PKT);
                for h in haves {
                    body.extend_from_slice(&pkt_line(format!("have {}\n", h.to_hex()).as_bytes()));
                }
                if !wants.is_empty() {
                    body.extend_from_slice(&pkt_line(b"done\n"));
                } else {
                    body.extend_from_slice(FLUSH_PKT);
                }
                let bytes = http_bytes(
                    "POST",
                    &format!("{base}/git-upload-pack"),
                    &body,
                    Some("application/x-git-upload-pack-request"),
                    user.as_deref(),
                    pass.as_deref(),
                )?;
                let pack_bytes = strip_pkt_lines(&bytes).to_vec();
                if pack_bytes.len() < 32 {
                    return Err("服务器未返回 pack（可能已是最新）".into());
                }
                Ok(pack_bytes)
            }
            Self::Ssh {
                user,
                host,
                port,
                path,
            } => {
                // 单次 SSH 会话完成：读广告 → 发 want/have/done → 收 pack
                let mut child = ssh_spawn(user, host, *port, "upload-pack", path)?;
                let mut stdout = child.stdout.take().expect("stdout 已管道化");
                let mut stdin = child.stdin.take().expect("stdin 已管道化");
                read_until_flush(&mut stdout).map_err(|e| format!("读取 SSH ref 广告失败: {e}"))?;

                use std::io::Write as _;
                let mut body = Vec::new();
                for (i, w) in wants.iter().enumerate() {
                    let caps = if i == 0 { " agent=mini-git/0.1" } else { "" };
                    body.extend_from_slice(&pkt_line(
                        format!("want {}{caps}\n", w.to_hex()).as_bytes(),
                    ));
                }
                body.extend_from_slice(FLUSH_PKT);
                for h in haves {
                    body.extend_from_slice(&pkt_line(format!("have {}\n", h.to_hex()).as_bytes()));
                }
                if !wants.is_empty() {
                    body.extend_from_slice(&pkt_line(b"done\n"));
                } else {
                    body.extend_from_slice(FLUSH_PKT);
                }
                stdin
                    .write_all(&body)
                    .map_err(|e| format!("写入 SSH 请求失败: {e}"))?;
                drop(stdin); // 明确收尾，避免个别服务端等待 EOF

                let mut rest = Vec::new();
                std::io::Read::read_to_end(&mut stdout, &mut rest)
                    .map_err(|e| format!("读取 SSH 响应失败: {e}"))?;
                let _ = child.wait();
                let pack_bytes = strip_pkt_lines(&rest).to_vec();
                if pack_bytes.len() < 32 {
                    return Err("服务器未返回 pack（可能已是最新）".into());
                }
                Ok(pack_bytes)
            }
        }
    }

    /// push：发送命令列表 + pack。
    pub fn send_pack(
        &self,
        updates: &[(Oid, Oid, String)], // (旧值, 新值, ref 名)
        pack_bytes: &[u8],
    ) -> Result<Vec<String>, String> {
        match self {
            Self::Local(path) => {
                let repo = Repo {
                    workdir: path.clone(),
                    gitdir: gitdir_of(path)?,
                };
                // 展开收到的 pack 写入远端，然后按 ff 规则更新 ref
                pack::unpack_to_loose(&repo.objects_dir(), pack_bytes, &|_| None)?;
                let mut report = Vec::new();
                for (old, new, name) in updates {
                    let current = repo
                        .read_ref(name)
                        .map(|o| o.to_hex())
                        .unwrap_or("0".repeat(40));
                    if current != old.to_hex() {
                        report.push(format!("ng {name} 非快进（远端为 {current}）"));
                        continue;
                    }
                    repo.write_ref(name, new)?;
                    report.push(format!("ok {name}"));
                }
                Ok(report)
            }
            Self::Http { base, user, pass } => {
                let mut body = Vec::new();
                for (i, (old, new, name)) in updates.iter().enumerate() {
                    let caps = if i == 0 {
                        "\0report-status agent=mini-git/0.1"
                    } else {
                        ""
                    };
                    body.extend_from_slice(&pkt_line(
                        format!("{} {} {name}{caps}\n", old.to_hex(), new.to_hex()).as_bytes(),
                    ));
                }
                body.extend_from_slice(FLUSH_PKT);
                body.extend_from_slice(pack_bytes);
                let bytes = http_bytes(
                    "POST",
                    &format!("{base}/git-receive-pack"),
                    &body,
                    Some("application/x-git-receive-pack-request"),
                    user.as_deref(),
                    pass.as_deref(),
                )?;
                // 响应是 pkt-line 文本（unpack ok / ok refs/... / ng ...）
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let mut report = Vec::new();
                for line in text.lines() {
                    let line = line.trim();
                    if line.len() >= 4 && line[..4].chars().all(|c| c.is_ascii_hexdigit()) {
                        let content = &line[4..];
                        if !content.is_empty() {
                            report.push(content.trim().to_string());
                        }
                    }
                }
                if report.is_empty() {
                    return Err(format!("push 响应异常：{text}"));
                }
                Ok(report)
            }
            Self::Ssh {
                user,
                host,
                port,
                path,
            } => {
                let mut child = ssh_spawn(user, host, *port, "receive-pack", path)?;
                let mut stdout = child.stdout.take().expect("stdout 已管道化");
                let mut stdin = child.stdin.take().expect("stdin 已管道化");
                read_until_flush(&mut stdout).map_err(|e| format!("读取 SSH ref 广告失败: {e}"))?;

                use std::io::Write as _;
                let mut body = Vec::new();
                for (i, (old, new, name)) in updates.iter().enumerate() {
                    let caps = if i == 0 {
                        "\0report-status agent=mini-git/0.1"
                    } else {
                        ""
                    };
                    body.extend_from_slice(&pkt_line(
                        format!("{} {} {name}{caps}\n", old.to_hex(), new.to_hex()).as_bytes(),
                    ));
                }
                body.extend_from_slice(FLUSH_PKT);
                body.extend_from_slice(pack_bytes);
                stdin
                    .write_all(&body)
                    .map_err(|e| format!("写入 SSH 推送失败: {e}"))?;
                drop(stdin);

                let mut rest = Vec::new();
                std::io::Read::read_to_end(&mut stdout, &mut rest)
                    .map_err(|e| format!("读取 SSH 推送结果失败: {e}"))?;
                let _ = child.wait();
                let text = String::from_utf8_lossy(&rest).into_owned();
                let mut report = Vec::new();
                for line in text.lines() {
                    let line = line.trim();
                    if line.len() >= 4 && line[..4].chars().all(|c| c.is_ascii_hexdigit()) {
                        let content = &line[4..];
                        if !content.is_empty() {
                            report.push(content.trim().to_string());
                        }
                    }
                }
                if report.is_empty() {
                    return Err(format!("push 响应异常：{text}"));
                }
                Ok(report)
            }
        }
    }
}

// ---- SSH（借道 ssh 命令；认证/密钥/别名走用户 ~/.ssh 配置）----

/// scp 风格：user@host:path（无 scheme，'@' 在首个 ':' 之前）。
fn is_scp_style(url: &str) -> bool {
    if url.contains("://") {
        return false;
    }
    match (url.find('@'), url.find(':')) {
        (Some(at), Some(colon)) => at < colon,
        _ => false,
    }
}

fn ssh_spawn(
    user: &Option<String>,
    host: &str,
    port: Option<u16>,
    service: &str,
    path: &str,
) -> Result<std::process::Child, String> {
    use std::process::{Command, Stdio};
    let dest = match user {
        Some(u) => format!("{u}@{host}"),
        None => host.to_string(),
    };
    let mut cmd = Command::new("ssh");
    cmd.arg(dest);
    if let Some(p) = port {
        cmd.args(["-p", &p.to_string()]);
    }
    // 在远端执行 git-upload-pack / git-receive-pack（路径相对家目录）
    cmd.arg(format!("git-{service} '{path}'"));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()); // 主机密钥确认 / 认证提示直接展示在终端
    cmd.spawn().map_err(|e| format!("启动 ssh 失败: {e}"))
}

/// 逐块读取直到出现 flush-pkt（ref 广告的结束标记）或 EOF。
fn read_until_flush(r: &mut impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.ends_with(b"0000") {
            break;
        }
    }
    Ok(buf)
}

// ---- HTTP（借道 curl）----

fn http_bytes(
    method: &str,
    url: &str,
    body: &[u8],
    content_type: Option<&str>,
    user: Option<&str>,
    pass: Option<&str>,
) -> Result<Vec<u8>, String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("curl");
    cmd.args(["-s", "--max-time", "120", "-X", method, url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(ct) = content_type {
        cmd.args(["-H", &format!("Content-Type: {ct}")]);
    }
    // 大请求体时 curl 会加 Expect: 100-continue 并等待，部分网关不放行 → 禁用
    cmd.args(["-H", "Expect:"]);
    if let (Some(u), Some(p)) = (user, pass) {
        cmd.args(["-u", &format!("{u}:{p}")]);
    }
    if method == "POST" {
        cmd.args(["--data-binary", "@-"]); // 请求体走 stdin，避开命令行长度限制
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 curl 失败（是否已安装？）: {e}"))?;
    if method == "POST" {
        child
            .stdin
            .take()
            .expect("stdin 已管道化")
            .write_all(body)
            .map_err(|e| format!("写入请求体失败: {e}"))?;
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("curl 退出码 {:?}", output.status.code()));
    }
    if std::env::var("MG_DEBUG").is_ok() {
        eprintln!(
            "HTTP {method} {url} -> {} bytes, 前 160 字节：{}",
            output.stdout.len(),
            String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(160)])
        );
    }
    Ok(output.stdout)
}

// ---- 辅助 ----

/// 本地路径可能是普通仓库（取其 .git）或 bare 仓库（本身即 gitdir）。
fn gitdir_of(path: &Path) -> Result<PathBuf, String> {
    let dot_git = path.join(".git");
    if dot_git.is_dir() {
        Ok(dot_git)
    } else if path.join("objects").is_dir() && path.join("HEAD").is_file() {
        Ok(path.to_path_buf())
    } else {
        Err(format!("不是 git 仓库：{}", path.display()))
    }
}

fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    p.to_string()
}

fn parse_ref_advertisement(body: &[u8]) -> Result<(Vec<RemoteRef>, String), String> {
    let mut refs = Vec::new();
    let mut caps = String::new();
    let mut buf = body;
    // 首行可能是 "# service=xxx"，跟一个 flush
    while buf.len() >= 4 {
        let head = &buf[..4];
        if head == FLUSH_PKT {
            buf = &buf[4..];
            continue;
        }
        if head == b"PACK" {
            break;
        }
        let Ok(len) = usize::from_str_radix(std::str::from_utf8(head).unwrap_or(""), 16) else {
            break;
        };
        if len == 0 || len > buf.len() {
            break;
        }
        let line = &buf[4..len];
        let line = std::str::from_utf8(line)
            .unwrap_or("")
            .trim_end()
            .to_string();
        buf = &buf[len..];
        if line.starts_with('#') {
            continue; // 服务声明行
        }
        // "{oid} {ref}\0{caps}" 或 "{oid} {ref}"
        let (pair, line_caps) = match line.split_once('\0') {
            Some((p, c)) => (p, c.to_string()),
            None => (line.as_str(), String::new()),
        };
        if let Some((oid_hex, name)) = pair.split_once(' ') {
            if !name.starts_with('^') {
                // 跳过 peeled 行
                if caps.is_empty() && !line_caps.is_empty() {
                    caps = line_caps;
                }
                if let Ok(oid) = Oid::from_hex(oid_hex) {
                    if oid.0 != [0; 20] {
                        refs.push(RemoteRef {
                            name: name.to_string(),
                            oid,
                        });
                    }
                }
            }
        }
    }
    Ok((refs, caps))
}

/// 从仓库收集 tip 可达、且不在 exclude 可达集中的全部对象（提交→树→blob）。
pub fn collect_reachable(
    repo: &Repo,
    tips: &[Oid],
    exclude: &[Oid],
) -> Result<Vec<(Oid, Obj)>, String> {
    // 通用遍历：从 tips 沿对象图走，遇到 stop 中的对象剪枝
    fn walk(repo: &Repo, tips: &[Oid], stop: &HashSet<Oid>) -> Result<Vec<(Oid, Obj)>, String> {
        let mut out = Vec::new();
        let mut seen: HashSet<Oid> = HashSet::new();
        let mut queue: Vec<Oid> = tips.to_vec();
        while let Some(oid) = queue.pop() {
            if stop.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            let obj = read_object(&repo.objects_dir(), &oid)?;
            match obj.kind {
                ObjKind::Commit => {
                    if let Ok(c) = parse_commit(&obj.data) {
                        queue.extend(c.parents);
                        queue.push(c.tree);
                    }
                }
                ObjKind::Tree => {
                    if let Ok(entries) = parse_tree(&obj.data) {
                        for e in entries {
                            queue.push(e.oid);
                        }
                    }
                }
                _ => {}
            }
            out.push((oid, obj));
        }
        Ok(out)
    }

    // 排除集：远端已有对象的可达闭包
    let have_set: HashSet<Oid> = if exclude.is_empty() {
        Default::default()
    } else {
        walk(repo, exclude, &Default::default())?
            .into_iter()
            .map(|(oid, _)| oid)
            .collect()
    };
    walk(repo, tips, &have_set)
}
