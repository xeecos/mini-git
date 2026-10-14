//! push：把当前分支推到 origin（构造无 delta pack，走 receive-pack）。

use crate::objects::{has_object, Oid};
use crate::pack;
use crate::repo::Repo;
use crate::transport::{collect_reachable, Transport};

pub fn push() -> Result<(), String> {
    let repo = Repo::discover(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    let branch = repo.current_branch()?;
    let url = repo.origin_url()?;
    let transport = Transport::parse(&url)?;

    let new_tip = repo
        .read_ref(&format!("refs/heads/{branch}"))
        .map_err(|_| "当前分支没有提交，先 commit".to_string())?;

    let (remote_refs, _caps) = transport.ref_discovery("receive-pack")?;
    let refname = format!("refs/heads/{branch}");
    let old_tip = remote_refs
        .iter()
        .find(|r| r.name == refname)
        .map(|r| r.oid);
    let old = old_tip.unwrap_or(Oid([0; 20]));

    if Some(new_tip) == old_tip {
        println!("远端已经是最新的");
        return Ok(());
    }

    // 远端已有的对象不用重发：取远端各 ref tip 中本地存在者作可达排除集
    let exclude: Vec<Oid> = remote_refs
        .iter()
        .map(|r| r.oid)
        .filter(|oid| has_object(&repo.objects_dir(), oid))
        .collect();
    let objects = collect_reachable(&repo, &[new_tip], &exclude)?;
    let pack_bytes = pack::write_pack(&objects);

    let report = transport.send_pack(&[(old, new_tip, refname.clone())], &pack_bytes)?;
    for line in &report {
        println!("{line}");
    }
    if report.iter().any(|l| l.starts_with("ng")) {
        return Err("推送被拒绝".into());
    }
    println!(
        "推送 {}：{} → {}（{} 个对象）",
        refname,
        &old.to_hex()[..7],
        &new_tip.to_hex()[..7],
        objects.len()
    );
    Ok(())
}
