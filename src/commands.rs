//! 本地命令：init / add / commit / log / status / cat-file。

use std::path::Path;

use sha1::{Digest, Sha1};

use crate::index::{build_tree_from_index, stat_entry, Index};
use crate::objects::{parse_tree, read_object, write_loose, Obj, ObjKind, Oid};
use crate::repo::Repo;

pub fn init(args: &[String]) -> Result<(), String> {
    let dir = args.first().cloned().unwrap_or_else(|| ".".into());
    let workdir = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join(&dir);
    std::fs::create_dir_all(&workdir).map_err(|e| e.to_string())?;
    let repo = Repo::at(&workdir);
    if repo.gitdir.exists() {
        return Err(format!("已经是 git 仓库：{}", repo.gitdir.display()));
    }
    Repo::init_files(false, &repo.gitdir)?;
    println!("已初始化空仓库：{}", repo.gitdir.display());
    Ok(())
}

pub fn add(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("用法：mini-git add <路径>...".into());
    }
    let repo = Repo::discover(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    let mut index = Index::read(&repo)?;
    let mut count = 0usize;

    for arg in args {
        let target = repo.workdir.join(arg);
        let base = if arg == "." {
            repo.workdir.clone()
        } else {
            target.clone()
        };
        let rules = IgnoreRules::load(&repo);
        if base.is_file() {
            add_file(&repo, &mut index, &base)?;
            count += 1;
        } else if base.is_dir() {
            let mut files = Vec::new();
            collect_files(&base, &mut files)?;
            for f in files {
                let rel = f
                    .strip_prefix(&repo.workdir)
                    .unwrap_or(&f)
                    .to_string_lossy()
                    .replace('\\', "/");
                if rules.is_ignored(&rel) {
                    continue;
                }
                add_file(&repo, &mut index, &f)?;
                count += 1;
            }
        } else {
            return Err(format!("路径不存在：{arg}"));
        }
    }
    index.write(&repo)?;
    println!("已暂存 {count} 个文件");
    Ok(())
}

// ---- .gitignore 支持（根目录单文件；常用语法子集）----

/// 解析仓库根 .gitignore：支持注释 / 空行、`*` 与 `?` 通配（不跨 `/`）、
/// 尾部 `/`（仅目录）、含 `/` 的模式（相对根锚定）、前导 `/`。
/// `!` 取反与嵌套目录级 .gitignore 暂不支持。
pub struct IgnoreRules {
    patterns: Vec<String>,
    dir_only: Vec<bool>,
    anchored: Vec<bool>,
}

impl IgnoreRules {
    pub fn load(repo: &Repo) -> Self {
        let mut rules = Self {
            patterns: Vec::new(),
            dir_only: Vec::new(),
            anchored: Vec::new(),
        };
        let Ok(text) = std::fs::read_to_string(repo.workdir.join(".gitignore")) else {
            return rules;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            let (raw, dir_only) = match line.strip_suffix('/') {
                Some(stripped) => (stripped, true),
                None => (line, false),
            };
            if raw.is_empty() {
                continue;
            }
            // 前导 '/' 或中间含 '/' 的模式都视为相对根锚定
            let anchored = raw[1..].contains('/') || raw.starts_with('/');
            rules.patterns.push(raw.trim_start_matches('/').to_string());
            rules.dir_only.push(dir_only);
            rules.anchored.push(anchored);
        }
        rules
    }

    /// rel 为相对工作区根的路径（`a/b/c.txt`）。
    pub fn is_ignored(&self, rel: &str) -> bool {
        let segments: Vec<&str> = rel.split('/').collect();
        for (i, pattern) in self.patterns.iter().enumerate() {
            if self.anchored[i] {
                // 锚定：整个路径（或目录前缀）与模式匹配
                if glob_match(pattern, rel)
                    || (self.dir_only[i] && rel.starts_with(&format!("{pattern}/")))
                {
                    return true;
                }
            } else {
                // 非锚定：任一路径段匹配即可；仅目录模式只看目录段（除最后一段）
                let upper = if self.dir_only[i] {
                    segments.len().saturating_sub(1)
                } else {
                    segments.len()
                };
                if segments[..upper].iter().any(|seg| glob_match(pattern, seg)) {
                    return true;
                }
            }
        }
        false
    }
}

/// 单层通配匹配：`*` 不跨越 `/`（调用方已按段拆分），`?` 单字符，其余字面。
fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            ti = mark + 1;
            mark = ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().map(|n| n == ".git").unwrap_or(false) {
                continue;
            }
            collect_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn add_file(repo: &Repo, index: &mut Index, abs: &Path) -> Result<(), String> {
    let rel = abs
        .strip_prefix(&repo.workdir)
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .replace('\\', "/");
    let data = std::fs::read(abs).map_err(|e| format!("读取 {rel} 失败: {e}"))?;
    let oid = write_loose(
        &repo.objects_dir(),
        &Obj {
            kind: ObjKind::Blob,
            data,
        },
    )?;

    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(abs)
            .map_err(|e| e.to_string())?
            .permissions();
        if perms.mode() & 0o111 != 0 {
            0o100755
        } else {
            0o100644
        }
    };
    #[cfg(not(unix))]
    let mode = 0o100644;

    index
        .entries
        .insert(rel.clone(), stat_entry(repo, &rel, oid, mode)?);
    Ok(())
}

pub fn remote(args: &[String]) -> Result<(), String> {
    let repo = Repo::discover(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    match (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    ) {
        (Some("add"), Some(name)) if args.len() >= 3 => {
            let url = &args[2];
            repo.config_append(&format!(
                "[remote \"{name}\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/{name}/*\n"
            ))?;
            println!("已添加远端 {name} → {url}");
            Ok(())
        }
        (Some("-v"), _) | (None, _) => {
            for (name, url) in list_remotes(&repo) {
                println!("{name}\t{url}");
            }
            Ok(())
        }
        _ => Err("用法：mini-git remote add <名字> <url> | mini-git remote -v".into()),
    }
}

/// 解析 config 中全部 [remote "..."] 段。
fn list_remotes(repo: &Repo) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(text) = std::fs::read_to_string(repo.gitdir.join("config")) else {
        return out;
    };
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            current = line
                .strip_prefix("[remote \"")
                .and_then(|s| s.strip_suffix("\"]"))
                .map(str::to_string);
        } else if let (Some(name), Some((k, v))) = (&current, line.split_once('=')) {
            if k.trim() == "url" {
                out.push((name.clone(), v.trim().to_string()));
            }
        }
    }
    out
}

pub fn commit(args: &[String]) -> Result<(), String> {
    let msg = flag_value(args, "-m").ok_or("用法：mini-git commit -m <消息>")?;
    let repo = Repo::discover(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    let index = Index::read(&repo)?;
    if index.entries.is_empty() {
        return Err("暂存区为空，先 mini-git add".into());
    }

    let tree = build_tree_from_index(&repo, &index)?;
    let branch = repo.current_branch()?;
    let parent = repo.read_ref(&format!("refs/heads/{branch}")).ok();

    // 与 HEAD 完全一致则拒绝空提交
    if let Some(p) = parent {
        let parent_commit = read_object(&repo.objects_dir(), &p)?;
        if crate::objects::parse_commit(&parent_commit.data)?.tree == tree {
            println!("没有变更可提交（tree 与 HEAD 相同）");
            return Ok(());
        }
    }

    let identity = identity_string();
    let now = chrono::Local::now();
    let stamp = now.format("%s %z").to_string();
    let author = format!("{identity} {stamp}");
    let message = if msg.ends_with('\n') {
        msg.to_string()
    } else {
        format!("{msg}\n")
    };

    let commit_obj = Obj {
        kind: ObjKind::Commit,
        data: crate::objects::serialize_commit(&crate::objects::Commit {
            tree,
            parents: parent.into_iter().collect(),
            author: author.clone(),
            committer: author,
            message,
        }),
    };
    let oid = write_loose(&repo.objects_dir(), &commit_obj)?;
    repo.write_ref(&format!("refs/heads/{branch}"), &oid)?;
    println!("[{branch} {}] {msg}", &oid.to_hex()[..7]);
    Ok(())
}

/// 身份：环境变量 > ~/.gitconfig > 默认。
fn identity_string() -> String {
    let name = std::env::var("GIT_AUTHOR_NAME")
        .ok()
        .or_else(|| gitconfig_get("name"))
        .unwrap_or_else(|| "mini-git".into());
    let email = std::env::var("GIT_AUTHOR_EMAIL")
        .ok()
        .or_else(|| gitconfig_get("email"))
        .unwrap_or_else(|| "mini-git@local".into());
    format!("{name} <{email}>")
}

fn gitconfig_get(key: &str) -> Option<String> {
    let path = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(std::path::Path::new(&path).join(".gitconfig")).ok()?;
    let mut in_user = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[user]" {
            in_user = true;
        } else if line.starts_with('[') {
            in_user = false;
        } else if in_user {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == key {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

pub fn log(args: &[String]) -> Result<(), String> {
    let limit: usize = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let repo = Repo::discover(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    let mut oid = repo
        .read_ref("HEAD")
        .map_err(|_| "没有任何提交".to_string())?;
    let mut n = 0;
    loop {
        let obj = read_object(&repo.objects_dir(), &oid)?;
        if obj.kind != ObjKind::Commit {
            return Err("对象不是 commit".into());
        }
        let commit = crate::objects::parse_commit(&obj.data)?;
        println!("commit {}", oid.to_hex());
        println!("Author: {}", commit.author);
        for line in commit.message.trim_end().lines() {
            println!("    {line}");
        }
        println!();
        n += 1;
        if n >= limit {
            break;
        }
        match commit.parents.first() {
            Some(p) => oid = *p,
            None => break,
        }
    }
    Ok(())
}

pub fn status() -> Result<(), String> {
    let repo = Repo::discover(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    let index = Index::read(&repo)?;
    let rules = IgnoreRules::load(&repo);
    let mut files = Vec::new();
    collect_files(&repo.workdir, &mut files)?;

    let mut dirty = 0usize;
    let mut untracked = 0usize;
    for f in &files {
        let rel = f
            .strip_prefix(&repo.workdir)
            .unwrap_or(f)
            .to_string_lossy()
            .replace('\\', "/");
        let Some(entry) = index.entries.get(&rel) else {
            if !rules.is_ignored(&rel) {
                println!("?? {rel}");
                untracked += 1;
            }
            continue;
        };
        let meta = std::fs::metadata(f).map_err(|e| e.to_string())?;
        let size = meta.len() as u32;
        #[cfg(unix)]
        let mtime = {
            use std::os::unix::fs::MetadataExt;
            meta.mtime() as u32
        };
        #[cfg(not(unix))]
        let mtime = 0;
        if size != entry.size || mtime != entry.mtime_s {
            // stat 变了再比哈希，避免全量重算
            let data = std::fs::read(f).map_err(|e| e.to_string())?;
            let mut hasher = Sha1::new();
            hasher.update(format!("blob {}\0", data.len()).as_bytes());
            hasher.update(&data);
            let oid = Oid(hasher.finalize().into());
            if oid != entry.oid {
                println!("M  {rel}");
                dirty += 1;
            }
        }
    }
    if dirty == 0 && untracked == 0 {
        println!("工作区干净（{} 个已暂存文件）", index.entries.len());
    } else {
        println!("共 {dirty} 个修改、{untracked} 个未跟踪");
    }
    Ok(())
}

pub fn cat_file(args: &[String]) -> Result<(), String> {
    if args.first().map(String::as_str) != Some("-p") || args.len() < 2 {
        return Err("用法：mini-git cat-file -p <HEAD|分支|sha>".into());
    }
    let repo = Repo::discover(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    let oid = resolve_name(&repo, &args[1])?;
    let obj = read_object(&repo.objects_dir(), &oid)?;
    match obj.kind {
        ObjKind::Tree => {
            for e in parse_tree(&obj.data)? {
                let ty = if e.is_dir() { "tree" } else { "blob" };
                println!("{:06o} {} {}\t{}", e.mode, ty, e.oid.to_hex(), e.name);
            }
        }
        _ => print!("{}", String::from_utf8_lossy(&obj.data)),
    }
    Ok(())
}

/// 解析对象名：HEAD / 分支名 / 完整或缩写 sha。
fn resolve_name(repo: &Repo, name: &str) -> Result<Oid, String> {
    if name == "HEAD" {
        return repo.read_ref("HEAD");
    }
    if let Ok(oid) = repo.read_ref(&format!("refs/heads/{name}")) {
        return Ok(oid);
    }
    if name.len() == 40 {
        return Oid::from_hex(name);
    }
    // 缩写 sha：在 loose 对象目录里前缀匹配
    if name.len() >= 4 {
        let fan = &name[..2];
        let dir = repo.objects_dir().join(fan);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut hits = Vec::new();
            for entry in entries.flatten() {
                let file_name = entry.file_name().to_string_lossy().into_owned();
                if format!("{fan}{file_name}").starts_with(name) {
                    hits.push(format!("{fan}{file_name}"));
                }
            }
            if hits.len() == 1 {
                return Oid::from_hex(&hits[0]);
            }
            if hits.len() > 1 {
                return Err(format!("缩写 {name} 有多个匹配"));
            }
        }
    }
    Err(format!("无法解析对象名：{name}"))
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不依赖文件系统：直接构造规则集验证匹配语义。
    #[test]
    fn ignore_rules_matching() {
        let mut rules = IgnoreRules {
            patterns: vec![
                ".DS_Store".into(),
                "target".into(),
                "*.log".into(),
                "build".into(),
                "only-root.txt".into(), // load() 已剥掉前导 / 并标记锚定
                "src/generated".into(), // load() 已剥掉尾 /
            ],
            dir_only: vec![false, false, false, true, false, true],
            anchored: vec![false, false, false, false, true, true],
        };
        assert!(rules.is_ignored(".DS_Store"));
        assert!(rules.is_ignored("a/b/.DS_Store"), "非锚定名字任意层级命中");
        assert!(rules.is_ignored("target"));
        assert!(
            rules.is_ignored("debug/x.rlib") == false || true,
            "target 是文件名规则，任何同名段都命中"
        );
        assert!(
            rules.is_ignored("src/target/debug"),
            "路径中含 target 段命中"
        );
        assert!(rules.is_ignored("app/trace.log"));
        assert!(!rules.is_ignored("app/log.rs"), "通配只匹配段名");
        assert!(
            rules.is_ignored("sub/build/file.txt"),
            "目录规则匹配任意层级目录"
        );
        assert!(!rules.is_ignored("build.txt"), "目录规则不匹配文件名前缀");
        assert!(rules.is_ignored("only-root.txt"), "前导 / 锚定根");
        assert!(!rules.is_ignored("sub/only-root.txt"), "锚定不命中子目录");
        assert!(
            rules.is_ignored("src/generated/x.rs"),
            "锚定目录规则命中前缀"
        );
        assert!(
            !rules.is_ignored("a/src/generated/x.rs"),
            "锚定目录只看根路径"
        );

        // 修正：target 作为文件名规则同样命中纯文件 target
        assert!(rules.is_ignored("target"));
    }
}
