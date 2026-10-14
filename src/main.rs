//! mini-git —— 纯 Rust 的 git 兼容子集。
//!
//! 目标不是替代 git（那有 ~30 万行 C），而是证明核心机制可以自举实现：
//! 对象模型（blob/tree/commit + SHA-1 + zlib loose 存储）、index v2、
//! packfile（读：含 ofs/ref-delta；写：无 delta）、smart HTTP v1 协议
//! （clone/pull/push），以及本地路径传输。
//!
//! 已验证与真 git 互操作：本工具创建的仓库能被 `git fsck/log` 读取，
//! 推送目标能被真 git 克隆，反之亦然。生产级需求请用 git 或 gitoxide（gix）。

mod commands;
mod fetch;
mod index;
mod objects;
mod pack;
mod push;
mod repo;
mod transport;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let rest = &args[1..];

    let result = match cmd {
        "init" => commands::init(rest),
        "add" => commands::add(rest),
        "remote" => commands::remote(rest),
        "commit" => commands::commit(rest),
        "log" => commands::log(rest),
        "status" => commands::status(),
        "cat-file" => commands::cat_file(rest),
        "clone" => match (rest.first(), rest.get(1)) {
            (Some(url), dir) => fetch::clone(url, dir.map(String::as_str)),
            (None, _) => Err("用法：mini-git clone <url> [目录]".to_string()),
        },
        "pull" => fetch::pull(),
        "push" => push::push(),
        "-h" | "--help" | "help" | "" => {
            print_help();
            Ok(())
        }
        other => Err(format!("未知命令：{other}（见 mini-git help）")),
    };

    if let Err(err) = result {
        eprintln!("mini-git: {err}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "mini-git —— 纯 Rust 的 git 兼容子集

用法：mini-git <命令> [参数]

本地命令：
  init [目录]             初始化仓库（默认分支 main）
  add <路径>...           暂存文件（递归目录，忽略删除）
  commit -m <消息>        从 index 生成 tree/commit 并更新分支
  log [条数]              沿第一父提交回溯
  status                  工作区 vs index 概览
  cat-file -p <对象>      查看对象内容（支持 HEAD / sha / 分支名）

网络命令（smart HTTP v1 / SSH / 本地路径）：
  clone <url> [目录]      克隆（支持 https:// 、user@host:path 、本地路径）
  pull                    拉取 origin 当前分支，仅快进合并
  push                    推送当前分支到 origin

认证：HTTPS 用 URL 内嵌 user:pass 或 MINI_GIT_USER/MINI_GIT_PASS（Basic）；
      SSH（git@host:path）直接使用你的 ~/.ssh 密钥与配置，无需额外设置。
限制：无 merge/rebase/diff/分支管理；pull 只做快进；未实现 .gitignore。"
    );
}
