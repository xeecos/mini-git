//! git index（暂存区）v2 的读写与 checkout。
//! v2 条目：62 字节定长头（ctime×2 mtime×2 dev ino mode uid gid size）+
//! 20 字节 oid + 2 字节 flags + 路径 + NUL 填充到 8 字节对齐；文件尾附 SHA-1。

use sha1::{Digest, Sha1};
use std::collections::BTreeMap;

use crate::objects::{parse_tree, read_object, serialize_tree, ObjKind, Oid, TreeEntry};
use crate::repo::Repo;

#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub ctime_s: u32,
    pub ctime_n: u32,
    pub mtime_s: u32,
    pub mtime_n: u32,
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub oid: Oid,
    pub path: String,
}

/// 路径 → 条目（BTreeMap 保证按路径排序，与 git index 一致）。
#[derive(Default)]
pub struct Index {
    pub entries: BTreeMap<String, IndexEntry>,
}

impl Index {
    pub fn read(repo: &Repo) -> Result<Self, String> {
        let path = repo.gitdir.join("index");
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(_) => return Ok(Self::default()), // 无 index 视为空（init 后未 add）
        };
        if raw.len() < 12 + 20 || &raw[..4] != b"DIRC" {
            return Err("index 头非法".into());
        }
        let version = u32::from_be_bytes(raw[4..8].try_into().unwrap());
        if version != 2 {
            return Err(format!("仅支持 index v2（当前 v{version}）"));
        }
        let count = u32::from_be_bytes(raw[8..12].try_into().unwrap()) as usize;
        // 校验尾 SHA-1
        let expect = &raw[raw.len() - 20..];
        let actual = Sha1::digest(&raw[..raw.len() - 20]);
        if actual.as_slice() != expect {
            return Err("index 校验和不匹配".into());
        }
        let mut idx = Self::default();
        let mut pos = 12usize;
        for _ in 0..count {
            let be = |pos: &mut usize| -> u32 {
                let v = u32::from_be_bytes(raw[*pos..*pos + 4].try_into().unwrap());
                *pos += 4;
                v
            };
            let ctime_s = be(&mut pos);
            let ctime_n = be(&mut pos);
            let mtime_s = be(&mut pos);
            let mtime_n = be(&mut pos);
            let dev = be(&mut pos);
            let ino = be(&mut pos);
            let mode = be(&mut pos);
            let uid = be(&mut pos);
            let gid = be(&mut pos);
            let size = be(&mut pos);
            let mut oid = [0u8; 20];
            oid.copy_from_slice(&raw[pos..pos + 20]);
            pos += 20;
            let flags = u16::from_be_bytes(raw[pos..pos + 2].try_into().unwrap());
            pos += 2;
            let name_len = (flags & 0x0FFF) as usize;
            // 条目定长头共 62 字节（stat40 + oid20 + flags2），name 紧随其后
            let entry_start = pos - 62;
            let name_end = if name_len < 0x0FFF {
                pos + name_len
            } else {
                raw[pos..]
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or("index 路径缺少 NUL")?
                    + pos
            };
            let path =
                String::from_utf8(raw[pos..name_end].to_vec()).map_err(|_| "index 路径非 UTF-8")?;
            let entry_len = name_end - entry_start;
            pos = entry_start + ((entry_len + 8) & !7); // 8 字节对齐（含至少 1 个 NUL）
            idx.entries.insert(
                path.clone(),
                IndexEntry {
                    ctime_s,
                    ctime_n,
                    mtime_s,
                    mtime_n,
                    dev,
                    ino,
                    mode,
                    uid,
                    gid,
                    size,
                    oid: Oid(oid),
                    path,
                },
            );
        }
        Ok(idx)
    }

    pub fn write(&self, repo: &Repo) -> Result<(), String> {
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for e in self.entries.values() {
            let entry_start = out.len();
            for v in [
                e.ctime_s, e.ctime_n, e.mtime_s, e.mtime_n, e.dev, e.ino, e.mode, e.uid, e.gid,
                e.size,
            ] {
                out.extend_from_slice(&v.to_be_bytes());
            }
            out.extend_from_slice(&e.oid.0);
            let name = e.path.as_bytes();
            let flags = name.len().min(0x0FFF) as u16;
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(name);
            let entry_len = out.len() - entry_start;
            // 填充 NUL 到 8 字节对齐且至少 1 个
            let padded = (entry_len + 8) & !7;
            out.resize(entry_start + padded, 0);
        }
        let checksum = Sha1::digest(&out);
        out.extend_from_slice(checksum.as_slice());
        std::fs::write(repo.gitdir.join("index"), out).map_err(|e| e.to_string())
    }
}

/// 用工作区文件的 stat 信息生成 index 条目元数据。
pub fn stat_entry(repo: &Repo, path: &str, oid: Oid, mode: u32) -> Result<IndexEntry, String> {
    #[cfg(unix)]
    let meta = {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(repo.workdir.join(path)).map_err(|e| e.to_string())?;
        (
            m.ctime() as u32,
            m.ctime_nsec() as u32,
            m.mtime() as u32,
            m.mtime_nsec() as u32,
            m.dev() as u32,
            m.ino() as u32,
            m.uid(),
            m.gid(),
            m.size() as u32,
        )
    };
    #[cfg(not(unix))]
    let meta = (0, 0, 0, 0, 0, 0, 0, 0, 0u32);

    Ok(IndexEntry {
        ctime_s: meta.0,
        ctime_n: meta.1,
        mtime_s: meta.2,
        mtime_n: meta.3,
        dev: meta.4,
        ino: meta.5,
        mode,
        uid: meta.6,
        gid: meta.7,
        size: meta.8,
        oid,
        path: path.to_string(),
    })
}

/// 把 tree 递归展开到工作区，并重建 index（clone / pull 快进后调用）。
pub fn checkout_tree(repo: &Repo, tree_oid: &Oid) -> Result<Index, String> {
    let mut index = Index::default();
    write_tree_entry(repo, tree_oid, "", &mut index)?;
    index.write(repo)?;
    Ok(index)
}

fn write_tree_entry(
    repo: &Repo,
    tree_oid: &Oid,
    prefix: &str,
    index: &mut Index,
) -> Result<(), String> {
    let obj = read_object(&repo.objects_dir(), tree_oid)?;
    if obj.kind != ObjKind::Tree {
        return Err(format!("对象 {} 不是 tree", tree_oid.to_hex()));
    }
    for entry in parse_tree(&obj.data)? {
        let full_path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        if entry.is_dir() {
            std::fs::create_dir_all(repo.workdir.join(&full_path))
                .map_err(|e| format!("创建目录失败 {full_path}: {e}"))?;
            write_tree_entry(repo, &entry.oid, &full_path, index)?;
        } else {
            let blob = read_object(&repo.objects_dir(), &entry.oid)?;
            let target = repo.workdir.join(&full_path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&target, &blob.data).map_err(|e| format!("写出文件失败: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if entry.mode & 0o111 != 0 {
                    let _ =
                        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755));
                }
            }
            index.entries.insert(
                full_path.clone(),
                stat_entry(repo, &full_path, entry.oid, entry.mode)?,
            );
        }
    }
    Ok(())
}

/// 从 index 构建（嵌套的）tree 对象，返回顶层 tree oid。
pub fn build_tree_from_index(repo: &Repo, index: &Index) -> Result<Oid, String> {
    fn build(repo: &Repo, index: &Index, prefix: &str) -> Result<Oid, String> {
        let mut entries: Vec<TreeEntry> = Vec::new();
        let mut subdirs: std::collections::BTreeSet<String> = Default::default();
        for (path, entry) in &index.entries {
            // 不属于当前子目录的路径直接跳过（顶层调用 prefix 为空，恒命中）
            let Some(rel) = path.strip_prefix(prefix) else {
                continue;
            };
            match rel.split_once('/') {
                Some((dir, _)) => {
                    subdirs.insert(dir.to_string());
                }
                None => {
                    entries.push(TreeEntry {
                        mode: entry.mode,
                        name: rel.to_string(),
                        oid: entry.oid,
                    });
                }
            }
        }
        for dir in subdirs {
            let sub_prefix = if prefix.is_empty() {
                format!("{dir}/")
            } else {
                format!("{prefix}{dir}/")
            };
            let oid = build(repo, index, &sub_prefix)?;
            entries.push(TreeEntry {
                mode: 0o40000,
                name: dir,
                oid,
            });
        }
        let data = serialize_tree(&mut entries);
        crate::objects::write_loose(
            &repo.objects_dir(),
            &crate::objects::Obj {
                kind: ObjKind::Tree,
                data,
            },
        )
    }
    build(repo, index, "")
}
