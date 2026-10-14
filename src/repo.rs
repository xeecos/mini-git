//! 仓库布局：.git 目录、refs、HEAD、config 的读写。

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::objects::Oid;

pub const DEFAULT_BRANCH: &str = "main";

pub struct Repo {
    /// 工作区根目录
    pub workdir: PathBuf,
    /// .git 目录
    pub gitdir: PathBuf,
}

impl Repo {
    /// 从当前目录（或指定目录）向上查找 .git。
    pub fn discover(dir: &Path) -> Result<Self, String> {
        let mut cur = dir.to_path_buf();
        loop {
            let git = cur.join(".git");
            if git.is_dir() {
                return Ok(Self {
                    workdir: cur.clone(),
                    gitdir: git,
                });
            }
            if !cur.pop() {
                return Err(format!("不在 git 仓库中：{}", dir.display()));
            }
        }
    }

    pub fn at(workdir: &Path) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
            gitdir: workdir.join(".git"),
        }
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.gitdir.join("objects")
    }

    // ---- refs ----

    /// 读取 ref 文件指向的 oid；支持传入完整 refs/ 路径。
    pub fn read_ref(&self, refname: &str) -> Result<Oid, String> {
        if refname == "HEAD" {
            let head = std::fs::read_to_string(self.gitdir.join("HEAD"))
                .map_err(|e| format!("读取 HEAD 失败: {e}"))?;
            if let Some(target) = head.strip_prefix("ref: ") {
                return self.read_ref(target.trim());
            }
            return Oid::from_hex(head.trim());
        }
        let path = self.gitdir.join(refname);
        let content =
            std::fs::read_to_string(&path).map_err(|e| format!("读取 ref {refname} 失败: {e}"))?;
        Oid::from_hex(content.trim())
    }

    /// 写 ref（自动创建父目录）。
    pub fn write_ref(&self, refname: &str, oid: &Oid) -> Result<(), String> {
        let path = self.gitdir.join(refname);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, format!("{}\n", oid.to_hex()))
            .map_err(|e| format!("写入 ref {refname} 失败: {e}"))
    }

    /// 列出全部 ref：松散 refs + packed-refs（bare 克隆常把 ref 打包进单文件）。
    pub fn list_refs(&self) -> Vec<(String, Oid)> {
        let mut out = Vec::new();
        let mut seen: std::collections::HashSet<String> = Default::default();
        for prefix in ["refs/heads", "refs/tags"] {
            let mut stack = vec![self.gitdir.join(prefix)];
            while let Some(d) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&d) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if let Ok(hex) = std::fs::read_to_string(&path) {
                        if let Ok(oid) = Oid::from_hex(hex.trim()) {
                            let rel = path
                                .strip_prefix(&self.gitdir)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .replace('\\', "/");
                            seen.insert(rel.clone());
                            out.push((rel, oid));
                        }
                    }
                }
            }
        }
        if let Ok(text) = std::fs::read_to_string(self.gitdir.join("packed-refs")) {
            for line in text.lines() {
                if line.starts_with('#') || line.starts_with('^') {
                    continue; // 头部说明 / peeled 行
                }
                if let Some((hex, name)) = line.split_once(' ') {
                    if let Ok(oid) = Oid::from_hex(hex) {
                        if seen.insert(name.to_string()) {
                            out.push((name.to_string(), oid));
                        }
                    }
                }
            }
        }
        out
    }

    /// HEAD 指向的分支名（如 main）。
    pub fn current_branch(&self) -> Result<String, String> {
        let head = std::fs::read_to_string(self.gitdir.join("HEAD"))
            .map_err(|e| format!("读取 HEAD 失败: {e}"))?;
        head.strip_prefix("ref: refs/heads/")
            .map(|s| s.trim().to_string())
            .ok_or_else(|| "HEAD 处于分离状态".to_string())
    }

    // ---- config（朴素的 ini 子集）----

    pub fn config_get(&self, section: &str, key: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.gitdir.join("config")).ok()?;
        let mut current = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                current = line[1..line.len() - 1].to_string();
            } else if current == section {
                if let Some((k, v)) = line.split_once('=') {
                    if k.trim() == key {
                        return Some(v.trim().to_string());
                    }
                }
            }
        }
        None
    }

    pub fn config_append(&self, text: &str) -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.gitdir.join("config"))
            .map_err(|e| e.to_string())?;
        writeln!(file, "{text}").map_err(|e| e.to_string())
    }

    /// origin 远端的 URL。
    pub fn origin_url(&self) -> Result<String, String> {
        self.config_get("remote \"origin\"", "url")
            .ok_or_else(|| "未配置 remote origin".to_string())
    }

    pub fn init_files(bare: bool, gitdir: &Path) -> Result<(), String> {
        for dir in ["objects", "refs/heads", "refs/tags"] {
            std::fs::create_dir_all(gitdir.join(dir)).map_err(|e| e.to_string())?;
        }
        std::fs::write(
            gitdir.join("HEAD"),
            format!("ref: refs/heads/{DEFAULT_BRANCH}\n"),
        )
        .map_err(|e| e.to_string())?;
        let filemode = if cfg!(unix) { "true" } else { "false" };
        std::fs::write(
            gitdir.join("config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tfilemode = {filemode}\n\tbare = {}\n",
                if bare { "true" } else { "false" }
            ),
        )
        .map_err(|e| e.to_string())
    }
}
