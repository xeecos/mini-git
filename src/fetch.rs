//! clone / pull（仅快进）。

use crate::index::checkout_tree;
use crate::objects::{parse_commit, read_object, ObjKind, Oid};
use crate::repo::{Repo, DEFAULT_BRANCH};
use crate::transport::Transport;

/// 克隆：拉取远端全部分支，HEAD 指向远端默认分支。
pub fn clone(url: &str, dir: Option<&str>) -> Result<(), String> {
    let transport = Transport::parse(url)?;
    let (refs, caps) = transport.ref_discovery("upload-pack")?;
    if refs.is_empty() {
        return Err("远端是空仓库，没有可克隆的 ref".into());
    }

    // 目标目录
    let dir = match dir {
        Some(d) => d.to_string(),
        None => default_dir_from_url(url),
    };
    let workdir = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join(&dir);
    if workdir.exists()
        && std::fs::read_dir(&workdir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    {
        return Err(format!("目录 {} 非空", workdir.display()));
    }
    std::fs::create_dir_all(&workdir).map_err(|e| e.to_string())?;
    let repo = Repo::at(&workdir);
    Repo::init_files(false, &repo.gitdir)?;

    // 默认分支：优先 symref 能力，其次唯一分支，最后 main
    let default_ref = caps
        .split(' ')
        .find_map(|c| c.strip_prefix("symref=HEAD:"))
        .map(str::to_string)
        .unwrap_or_else(|| {
            let heads: Vec<&str> = refs
                .iter()
                .map(|r| r.name.as_str())
                .filter(|n| n.starts_with("refs/heads/"))
                .collect();
            if heads.len() == 1 {
                heads[0].to_string()
            } else {
                format!("refs/heads/{DEFAULT_BRANCH}")
            }
        });

    // 拉取（克隆无 haves；瘦包不会出现——服务器无公共祖先可省）。
    // 只 want 分支与标签：refs/pull/* 等特殊命名空间的量可达上千，
    // 全部 want 会让服务器组包超时（真 git 同样只取 heads+tags）。
    let keep: Vec<&crate::transport::RemoteRef> = refs
        .iter()
        .filter(|r| r.name.starts_with("refs/heads/") || r.name.starts_with("refs/tags/"))
        .collect();
    let wants: Vec<Oid> = keep.iter().map(|r| r.oid).collect();
    let pack_bytes = transport.fetch_pack(&wants, &[])?;
    let written = crate::pack::unpack_to_loose(&repo.objects_dir(), &pack_bytes, &|_| None)?;
    for r in &keep {
        repo.write_ref(&r.name, &r.oid)?;
    }

    // HEAD 指向默认分支（若该分支不在远端 ref 中则退回第一个分支）
    let head_ref = if keep.iter().any(|r| r.name == default_ref) {
        default_ref
    } else {
        keep.first().map(|r| r.name.clone()).ok_or("远端没有分支")?
    };
    std::fs::write(repo.gitdir.join("HEAD"), format!("ref: {head_ref}\n"))
        .map_err(|e| e.to_string())?;
    let branch = head_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(DEFAULT_BRANCH);

    repo.config_append(&format!(
        "[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/heads/*\n"
    ))?;
    repo.config_append(&format!(
        "[branch \"{branch}\"]\n\tremote = origin\n\tmerge = refs/heads/{branch}\n"
    ))?;

    // 展开工作区
    let tip = repo.read_ref(&head_ref)?;
    checkout_tree(&repo, &tree_of(&repo, &tip)?)?;

    println!(
        "克隆完成：{url} → {}（{written} 个对象，分支 {branch}）",
        workdir.display()
    );
    Ok(())
}

/// pull：取回 origin 当前分支并快进（非快进则报错，提示分叉）。
pub fn pull() -> Result<(), String> {
    let repo = Repo::discover(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    let branch = repo.current_branch()?;
    let url = repo.origin_url()?;
    let transport = Transport::parse(&url)?;

    let (refs, _caps) = transport.ref_discovery("upload-pack")?;
    let remote_tip = refs
        .iter()
        .find(|r| r.name == format!("refs/heads/{branch}"))
        .map(|r| r.oid)
        .ok_or_else(|| format!("远端没有分支 {branch}"))?;

    let local_tip = repo
        .read_ref(&format!("refs/heads/{branch}"))
        .map_err(|_| format!("本地没有分支 {branch}，请先 clone"))?;

    if local_tip == remote_tip {
        println!("已经是最新的");
        return Ok(());
    }

    let pack_bytes = transport.fetch_pack(&[remote_tip], &[local_tip])?;
    // 瘦包的基对象可能在本地，提供回查
    let objects_dir = repo.objects_dir();
    let written = crate::pack::unpack_to_loose(&objects_dir, &pack_bytes, &|oid| {
        read_object(&objects_dir, oid).ok()
    })?;

    if !is_ancestor(&repo, &local_tip, &remote_tip)? {
        return Err(format!(
            "本地与远端已分叉（拉取了 {written} 个对象但拒绝合并）；本工具仅支持快进，请用真 git 处理分叉"
        ));
    }

    repo.write_ref(&format!("refs/heads/{branch}"), &remote_tip)?;
    checkout_tree(&repo, &tree_of(&repo, &remote_tip)?)?;
    println!(
        "快进到 {}（{written} 个对象）",
        &remote_tip.to_hex()[..7.min(40)]
    );
    Ok(())
}

/// child 是否是 ancestor 的后代（沿第一父链搜索）。
pub fn is_ancestor(repo: &Repo, ancestor: &Oid, child: &Oid) -> Result<bool, String> {
    let mut queue = vec![*child];
    let mut seen = std::collections::HashSet::new();
    while let Some(oid) = queue.pop() {
        if oid == *ancestor {
            return Ok(true);
        }
        if !seen.insert(oid) {
            continue;
        }
        if let Ok(obj) = read_object(&repo.objects_dir(), &oid) {
            if obj.kind == ObjKind::Commit {
                if let Ok(c) = parse_commit(&obj.data) {
                    queue.extend(c.parents);
                }
            }
        }
    }
    Ok(false)
}

fn tree_of(repo: &Repo, commit: &Oid) -> Result<Oid, String> {
    let obj = read_object(&repo.objects_dir(), commit)?;
    Ok(parse_commit(&obj.data)?.tree)
}

fn default_dir_from_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or("repo");
    name.trim_end_matches(".git").to_string()
}
