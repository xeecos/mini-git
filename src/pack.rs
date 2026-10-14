//! packfile：读（v2，含 ofs-delta / ref-delta 解析，支持瘦包）与写（无 delta）。
//!
//! 解析后的对象全部以 loose 形式落盘——代价是磁盘占用，换来实现简单、
//! 且产物可被真 git 直接读取（互操作测试的关键）。

use std::collections::HashMap;

use sha1::{Digest, Sha1};

use crate::objects::{write_loose, Obj, ObjKind, Oid};

/// 解析 pack：返回全部对象（含 delta 展开）。`lookup` 供瘦包的缺失基对象回查。
pub fn parse_pack(pack: &[u8], lookup: &dyn Fn(&Oid) -> Option<Obj>) -> Result<Vec<Obj>, String> {
    if pack.len() < 32 || &pack[..4] != b"PACK" {
        return Err("不是合法的 packfile".into());
    }
    let count = u32::from_be_bytes(pack[8..12].try_into().unwrap()) as usize;
    // 尾部校验和 = 除末 20 字节外全部内容的 SHA-1
    let expect = &pack[pack.len() - 20..];
    let actual = Sha1::digest(&pack[..pack.len() - 20]);
    if actual.as_slice() != expect {
        return Err("pack 校验和不匹配".into());
    }

    // ---- 第一遍：逐条目读出（正文解压，delta 暂不展开）----
    struct Raw {
        kind_code: u8,
        data: Vec<u8>,
        base_offset: Option<u64>,
        base_oid: Option<Oid>,
    }
    let mut entries: Vec<(u64, Raw)> = Vec::with_capacity(count);
    let mut by_offset: HashMap<u64, usize> = HashMap::with_capacity(count);

    let mut pos = 12usize;
    for _ in 0..count {
        let offset = pos as u64;
        let first = pack[pos];
        pos += 1;
        let kind_code = (first >> 4) & 0x7;
        while pack[pos - 1] & 0x80 != 0 {
            pos += 1; // 尺寸变长的后续字节（长度由 zlib 流本身界定，无需使用）
        }
        let (base_offset, base_oid) = match kind_code {
            6 => {
                // ofs-delta：大端风格变长负偏移，基对象一定在更早的偏移处
                let mut b = pack[pos];
                pos += 1;
                let mut off = (b & 0x7F) as u64;
                while b & 0x80 != 0 {
                    b = pack[pos];
                    pos += 1;
                    off = ((off + 1) << 7) | (b & 0x7F) as u64;
                }
                (Some(offset - off), None)
            }
            7 => {
                let mut oid = [0u8; 20];
                oid.copy_from_slice(&pack[pos..pos + 20]);
                pos += 20;
                (None, Some(Oid(oid)))
            }
            _ => (None, None),
        };
        // bufread 变体按需拉取：total_in 精确等于该条目消耗的压缩字节数
        // （read 变体自带预读缓冲，会多算导致后续偏移漂移）
        let mut decoder = flate2::bufread::ZlibDecoder::new(&pack[pos..]);
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut data)
            .map_err(|e| format!("pack 条目解压失败: {e}"))?;
        pos += decoder.total_in() as usize;
        if std::env::var("MG_DEBUG").is_ok() {
            eprintln!("entry off={offset} kind={kind_code} base_off={base_offset:?} base_oid={} datalen={} consumed_to={pos}", base_oid.as_ref().map(|o| o.to_hex()).unwrap_or_default(), data.len());
        }
        by_offset.insert(offset, entries.len());
        entries.push((
            offset,
            Raw {
                kind_code,
                data,
                base_offset,
                base_oid,
            },
        ));
    }

    // ---- 第二遍：解析。先展开全部直接对象建 oid 索引，再定点迭代展开 delta ----
    let mut resolved: Vec<Option<Obj>> = vec![None; entries.len()];
    let mut by_oid: HashMap<Oid, usize> = HashMap::with_capacity(count);

    for idx in 0..entries.len() {
        let (_, raw) = &entries[idx];
        if (1..=4).contains(&raw.kind_code) {
            let obj = Obj {
                kind: ObjKind::from_code(raw.kind_code)?,
                data: raw.data.clone(),
            };
            let (oid, _) = obj.full();
            by_oid.insert(oid, idx);
            resolved[idx] = Some(obj);
        }
    }

    loop {
        let mut progress = false;
        for idx in 0..entries.len() {
            if resolved[idx].is_some() {
                continue;
            }
            let (_, raw) = &entries[idx];
            let base = if let Some(base_off) = raw.base_offset {
                let bidx = by_offset
                    .get(&base_off)
                    .copied()
                    .and_then(|i| resolved[i].clone())
                    .ok_or_else(|| format!("ofs-delta 基对象未就绪：偏移 {base_off}"))?;
                bidx
            } else if let Some(base_oid) = raw.base_oid {
                match by_oid.get(&base_oid).and_then(|i| resolved[*i].clone()) {
                    Some(b) => b,
                    None => match lookup(&base_oid) {
                        Some(b) => b,
                        None => continue, // 可能本轮之后出现；留待定点迭代
                    },
                }
            } else {
                return Err(format!("pack 类型码非法：{}", raw.kind_code));
            };
            let patched = apply_delta(&base.data, &raw.data)?;
            let obj = Obj {
                kind: base.kind,
                data: patched,
            };
            let (oid, _) = obj.full();
            by_oid.insert(oid, idx);
            resolved[idx] = Some(obj);
            progress = true;
        }
        if resolved.iter().all(|r| r.is_some()) {
            break;
        }
        if !progress {
            let missing: Vec<String> = resolved
                .iter()
                .enumerate()
                .filter(|(_, r)| r.is_none())
                .map(|(i, _)| entries[i].0.to_string())
                .collect();
            return Err(format!("pack 存在无法解析的 delta（基缺失）：{missing:?}"));
        }
    }

    let mut out_objs = Vec::with_capacity(entries.len());
    for obj in resolved.into_iter().flatten() {
        out_objs.push(obj);
    }
    Ok(out_objs)
}

/// 解析 pack 并把全部对象写成 loose（fetch 收包后调用），返回对象数。
pub fn unpack_to_loose(
    objects_dir: &std::path::Path,
    pack: &[u8],
    lookup: &dyn Fn(&Oid) -> Option<Obj>,
) -> Result<usize, String> {
    let objs = parse_pack(pack, lookup)?;
    let mut written = 0usize;
    for obj in &objs {
        write_loose(objects_dir, obj)?;
        written += 1;
    }
    Ok(written)
}

/// 应用 git delta 流（copy / insert 指令序列）。
fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let (src, mut p) = read_varint(delta, 0);
    let (dst, q) = read_varint(delta, p);
    p = q;
    if src as usize != base.len() {
        return Err(format!("delta 基长度不符：期望 {src}，实际 {}", base.len()));
    }
    let mut out = Vec::with_capacity(dst as usize);
    while p < delta.len() {
        let op = delta[p];
        p += 1;
        if op & 0x80 != 0 {
            // copy：位掩码指示偏移/长度的非零字节
            let mut off = 0u64;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    off |= (delta[p] as u64) << (8 * i);
                    p += 1;
                }
            }
            let mut len = 0u64;
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    len |= (delta[p] as u64) << (8 * i);
                    p += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            let start = off as usize;
            let end = start + len as usize;
            if end > base.len() {
                return Err("delta copy 越界".into());
            }
            out.extend_from_slice(&base[start..end]);
        } else if op != 0 {
            let end = p + op as usize;
            if end > delta.len() {
                return Err("delta insert 越界".into());
            }
            out.extend_from_slice(&delta[p..end]);
            p = end;
        } else {
            return Err("delta 指令码 0 非法".into());
        }
    }
    if out.len() != dst as usize {
        return Err(format!(
            "delta 结果长度不符：期望 {dst}，实际 {}",
            out.len()
        ));
    }
    Ok(out)
}

fn read_varint(buf: &[u8], mut pos: usize) -> (u64, usize) {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let b = buf[pos];
        pos += 1;
        value |= ((b & 0x7F) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
    }
    (value, pos)
}

// ---- 写 pack（无 delta）----

/// 把对象集合写成 v2 pack（无 delta，任何 git 服务端都能接收）。
pub fn write_pack(objects: &[(Oid, Obj)]) -> Vec<u8> {
    use std::io::Write;

    let mut out = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for (_, obj) in objects {
        write_object_header(&mut out, obj.kind.code(), obj.data.len() as u64);
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
        let _ = zlib.write_all(&obj.data);
        if let Ok(compressed) = zlib.finish() {
            out.extend_from_slice(&compressed);
        }
    }
    let checksum = Sha1::digest(&out);
    out.extend_from_slice(checksum.as_slice());
    out
}

fn write_object_header(out: &mut Vec<u8>, kind_code: u8, size: u64) {
    let mut b = (kind_code << 4) | ((size & 0x0F) as u8);
    let mut rest = size >> 4;
    loop {
        if rest == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
        b = (rest & 0x7F) as u8;
        rest >>= 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{Obj, ObjKind};

    /// 写出的 pack 必须能被自己的解析器读回（clone 本地路径流程的关键闭环）。
    #[test]
    fn write_then_parse_roundtrip() {
        let mut objects = Vec::new();
        for i in 0..50 {
            let data = format!("对象内容 #{i}\n").repeat(i + 1).into_bytes();
            let obj = Obj {
                kind: ObjKind::Blob,
                data,
            };
            let (oid, _) = obj.full();
            objects.push((oid, obj));
        }
        let tree_obj = Obj {
            kind: ObjKind::Commit,
            data: b"tree 0000000000000000000000000000000000000000\nauthor t <t@t> 1 +0000\ncommitter t <t@t> 1 +0000\n\nmsg\n".to_vec(),
        };
        let (toid, _) = tree_obj.full();
        objects.push((toid, tree_obj));

        let pack = write_pack(&objects);
        let parsed = parse_pack(&pack, &|_| None).expect("解析自写 pack");
        assert_eq!(parsed.len(), objects.len(), "对象数量不一致");
        for (want, got) in objects.iter().zip(parsed.iter()) {
            let (goid, _) = got.full();
            assert_eq!(&goid, &want.0, "oid 不一致");
            assert_eq!(got.kind.name(), want.1.kind.name());
            assert_eq!(got.data, want.1.data, "内容不一致");
        }
    }
}
