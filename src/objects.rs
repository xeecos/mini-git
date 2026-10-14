//! git 对象模型：blob / tree / commit / tag。
//! loose 对象 = zlib("类型 长度\0" + 内容)，文件名 = SHA-1 的十六进制。

use std::io::Write;
use std::path::PathBuf;

use sha1::{Digest, Sha1};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub fn from_hex(hex: &str) -> Result<Self, String> {
        let bytes = hex.as_bytes();
        if bytes.len() != 40 {
            return Err(format!("非法对象 id：{hex}"));
        }
        let mut out = [0u8; 20];
        // as_chunks 尚未稳定，这里用 chunks_exact
        #[allow(clippy::chunks_exact_to_as_chunks)]
        for (i, chunk) in bytes.chunks_exact(2).enumerate() {
            let hi = hex_val(chunk[0])?;
            let lo = hex_val(chunk[1])?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }

    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 对象的松散存储路径（objects/ab/cdef…）。
    pub fn loose_path(objects_dir: &std::path::Path, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        objects_dir.join(&hex[..2]).join(&hex[2..])
    }
}

fn hex_val(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(format!("非法十六进制字符：{}", c as char)),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjKind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Tag => "tag",
        }
    }

    /// pack 内的 3 位类型码。
    pub fn code(&self) -> u8 {
        match self {
            Self::Commit => 1,
            Self::Tree => 2,
            Self::Blob => 3,
            Self::Tag => 4,
        }
    }

    pub fn from_code(code: u8) -> Result<Self, String> {
        Ok(match code {
            1 => Self::Commit,
            2 => Self::Tree,
            3 => Self::Blob,
            4 => Self::Tag,
            _ => return Err(format!("未知对象类型码：{code}")),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Obj {
    pub kind: ObjKind,
    pub data: Vec<u8>,
}

impl Obj {
    /// 计算对象 id 并返回带头的完整内容。
    pub fn full(&self) -> (Oid, Vec<u8>) {
        let header = format!("{} {}\0", self.kind.name(), self.data.len());
        let mut full = Vec::with_capacity(header.len() + self.data.len());
        full.extend_from_slice(header.as_bytes());
        full.extend_from_slice(&self.data);
        let oid = Oid(Sha1::digest(&full).into());
        (oid, full)
    }
}

// ---- tree ----

#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub mode: u32, // 100644 / 100755 / 40000 / 120000 / 160000
    pub name: String,
    pub oid: Oid,
}

impl TreeEntry {
    pub fn is_dir(&self) -> bool {
        self.mode == 0o40000
    }
}

pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let space = data[pos..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or("tree 项缺少空格")?
            + pos;
        let mode = std::str::from_utf8(&data[pos..space])
            .ok()
            .and_then(|s| u32::from_str_radix(s, 8).ok())
            .ok_or("tree 项 mode 非法")?;
        let nul = data[space + 1..]
            .iter()
            .position(|&b| b == 0)
            .ok_or("tree 项缺少 NUL")?
            + space
            + 1;
        let name =
            String::from_utf8(data[space + 1..nul].to_vec()).map_err(|_| "tree 项名称非 UTF-8")?;
        if nul + 21 > data.len() {
            return Err("tree 项缺少 oid".into());
        }
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&data[nul + 1..nul + 21]);
        out.push(TreeEntry {
            mode,
            name,
            oid: Oid(oid),
        });
        pos = nul + 21;
    }
    Ok(out)
}

pub fn serialize_tree(entries: &mut [TreeEntry]) -> Vec<u8> {
    // git 排序规则：按名字节比较，目录视作带 '/' 后缀
    entries.sort_by(|a, b| {
        let mut an = a.name.clone().into_bytes();
        let mut bn = b.name.clone().into_bytes();
        if a.is_dir() {
            an.push(b'/');
        }
        if b.is_dir() {
            bn.push(b'/');
        }
        an.cmp(&bn)
    });
    let mut out = Vec::new();
    for e in entries.iter() {
        out.extend_from_slice(format!("{:o}", e.mode).as_bytes());
        out.push(b' ');
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        out.extend_from_slice(&e.oid.0);
    }
    out
}

// ---- commit ----

#[derive(Clone, Debug)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: String,
    pub committer: String,
    pub message: String,
}

pub fn parse_commit(data: &[u8]) -> Result<Commit, String> {
    let text = std::str::from_utf8(data).map_err(|_| "commit 非 UTF-8")?;
    let (header, message) = text.split_once("\n\n").ok_or("commit 缺少消息分隔")?;
    let mut commit = Commit {
        tree: Oid([0; 20]),
        parents: Vec::new(),
        author: String::new(),
        committer: String::new(),
        message: message.to_string(),
    };
    for line in header.lines() {
        if let Some(v) = line.strip_prefix("tree ") {
            commit.tree = Oid::from_hex(v)?;
        } else if let Some(v) = line.strip_prefix("parent ") {
            commit.parents.push(Oid::from_hex(v)?);
        } else if let Some(v) = line.strip_prefix("author ") {
            commit.author = v.to_string();
        } else if let Some(v) = line.strip_prefix("committer ") {
            commit.committer = v.to_string();
        }
    }
    Ok(commit)
}

pub fn serialize_commit(c: &Commit) -> Vec<u8> {
    let mut out = format!("tree {}\n", c.tree.to_hex()).into_bytes();
    for p in &c.parents {
        out.extend_from_slice(format!("parent {}\n", p.to_hex()).as_bytes());
    }
    out.extend_from_slice(format!("author {}\n", c.author).as_bytes());
    out.extend_from_slice(format!("committer {}\n\n", c.committer).as_bytes());
    out.extend_from_slice(c.message.as_bytes());
    out
}

// ---- loose 对象读写 ----

/// 写 loose 对象（幂等：已存在则跳过），返回 oid。
pub fn write_loose(objects_dir: &std::path::Path, obj: &Obj) -> Result<Oid, String> {
    let (oid, full) = obj.full();
    let path = Oid::loose_path(objects_dir, &oid);
    if path.exists() {
        return Ok(oid);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    zlib.write_all(&full).map_err(|e| e.to_string())?;
    let compressed = zlib.finish().map_err(|e| e.to_string())?;
    std::fs::write(&path, compressed).map_err(|e| format!("写入对象失败: {e}"))?;
    Ok(oid)
}

/// 读 loose 对象。
pub fn read_loose(objects_dir: &std::path::Path, oid: &Oid) -> Result<Obj, String> {
    let path = Oid::loose_path(objects_dir, oid);
    let raw = std::fs::read(&path).map_err(|_| format!("对象不存在：{}", oid.to_hex()))?;
    let mut zlib = flate2::read::ZlibDecoder::new(&raw[..]);
    let mut full = Vec::new();
    std::io::Read::read_to_end(&mut zlib, &mut full).map_err(|e| format!("解压对象失败: {e}"))?;
    let nul = full.iter().position(|&b| b == 0).ok_or("对象头缺少 NUL")?;
    let header = std::str::from_utf8(&full[..nul]).map_err(|_| "对象头非 UTF-8")?;
    let (kind_str, _len) = header.split_once(' ').ok_or("对象头缺少长度")?;
    let kind = match kind_str {
        "commit" => ObjKind::Commit,
        "tree" => ObjKind::Tree,
        "blob" => ObjKind::Blob,
        "tag" => ObjKind::Tag,
        _ => return Err(format!("未知对象类型：{kind_str}")),
    };
    Ok(Obj {
        kind,
        data: full[nul + 1..].to_vec(),
    })
}

pub fn has_loose(objects_dir: &std::path::Path, oid: &Oid) -> bool {
    Oid::loose_path(objects_dir, oid).exists()
}

// ---- 统一读取层：loose 优先，miss 后解析目录下的 pack（整包缓存）----

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

type PackMap = Arc<HashMap<Oid, Obj>>;

fn pack_cache() -> &'static Mutex<HashMap<PathBuf, (u128, PackMap)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (u128, PackMap)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 解析 objects/pack 下全部 pack（按 pack 目录 mtime 缓存于进程内）。
fn load_pack_map(objects_dir: &Path) -> Result<PackMap, String> {
    let pack_dir = objects_dir.join("pack");
    let dir_stamp = std::fs::metadata(&pack_dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut cache = pack_cache().lock().unwrap();
    if let Some((stamp, map)) = cache.get(objects_dir) {
        if *stamp == dir_stamp {
            return Ok(Arc::clone(map));
        }
    }
    let mut merged: HashMap<Oid, Obj> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(&pack_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pack") {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            for obj in crate::pack::parse_pack(&bytes, &|_| None)? {
                let (oid, _) = obj.full();
                merged.insert(oid, obj);
            }
        }
    }
    let arc = Arc::new(merged);
    cache.insert(objects_dir.to_path_buf(), (dir_stamp, arc.clone()));
    Ok(arc)
}

/// 统一对象读取：先 loose，再 pack。
pub fn read_object(objects_dir: &Path, oid: &Oid) -> Result<Obj, String> {
    if has_loose(objects_dir, oid) {
        return read_loose(objects_dir, oid);
    }
    let packs = load_pack_map(objects_dir)?;
    packs
        .get(oid)
        .cloned()
        .ok_or_else(|| format!("对象不存在：{}", oid.to_hex()))
}

/// 统一存在性检查。
pub fn has_object(objects_dir: &Path, oid: &Oid) -> bool {
    if has_loose(objects_dir, oid) {
        return true;
    }
    load_pack_map(objects_dir)
        .map(|packs| packs.contains_key(oid))
        .unwrap_or(false)
}
