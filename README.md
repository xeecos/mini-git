# mini-git

用纯 Rust 从零实现的 git 兼容子集——不依赖任何 git 类 crate（只用 `sha1` / `flate2` / `chrono`，
HTTP 借道 `curl`、SSH 借道 `ssh`），从对象模型到网络协议完整自举，并与真 git **双向互操作验证通过**。

```bash
cargo build --release
./target/release/mini-git help

mini-git init / add / commit / log / status / cat-file   # 本地命令
mini-git clone <url> / pull / push                        # 网络（HTTPS / SSH / 本地路径）
mini-git remote add <名字> <url>
```

## 实现了什么

| 层 | 内容 |
|----|------|
| 对象模型 | blob / tree / commit 的 SHA-1 + zlib loose 读写、tree 组装（含 git 目录排序规则）、**git index v2 二进制格式** |
| packfile | v2 解析（ofs-delta / ref-delta 展开、瘦包基对象回查、整包校验和）+ 无 delta 写出 |
| smart HTTP | pkt-line 编码、ref 广告解析、want/have 协商、receive-pack 推送、增量排除公共对象 |
| SSH | `git@host:path` 与 `ssh://` 地址；复用 `~/.ssh` 的密钥 / 别名 / 配置，无需 token |
| 本地传输 | 裸仓库与普通仓库路径；packed-refs 读取；pack 中的对象直接解析 |

产物兼容性：mini-git 建立的仓库能被 `git fsck / log / status` 直接读取；mini-git 推送的目标能被
真 git 克隆；反之 mini-git 能克隆经过 `git gc` 打包（含 delta）的仓库。

## 互操作测试（21 项全部通过）

- mini-git 建仓 → 真 `git fsck` 零错误、`git log` 可读、`git status` 干净（index v2 兼容）
- mini-git push → 真 git 克隆逐字节一致
- 真 git 仓库 gc 打包 → mini-git clone：工作区 diff 一致、提交哈希序列完全一致
- 真 git 推进远端 → mini-git pull 快进成功（幂等 pull 正确）
- mini-git 增量 push → 真 git 检出并认可
- GitHub 实测：HTTPS 匿名克隆与 SSH 密钥克隆（octocat/Hello-World）

认证方式：HTTPS 用 URL 内嵌 `user:pass` 或环境变量 `MINI_GIT_USER` / `MINI_GIT_PASS`；
SSH 直接使用本机 ssh 配置。

## 设计取舍（骨架级）

- 解析后的 pack 全部落盘为 loose 对象（不写 .idx / .pack）
- 推送不生成 delta 压缩（服务端总能接收）
- `pull` 仅快进合并，分叉时明确拒绝；无 merge / rebase / diff / 分支管理 / .gitignore / shallow
- 瘦包仅支持基对象回查本地对象库

生产环境请使用 git 本体或 [gitoxide](https://github.com/GitoxideLMIT/gitoxide)；
本项目的价值是把 git 的核心机制（对象图、包格式、线上协议）完整走通一遍，全部代码约 2000 行，
可以逐行阅读。

## License

Apache-2.0
