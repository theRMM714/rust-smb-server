# rust-smb-server（fork）

本仓是 [paltaio/rust-smb-server](https://github.com/paltaio/rust-smb-server) 的 fork（MIT）。
保留它的目的有两个：

1. **保险**：上游删库、改名或归档时，Solomni 仍有可拉取的源码与可构建的 helper。
2. **预编译 smbd**：上游只发布 crate，不发 Windows 二进制；Solomni 的 Windows 虚拟机档需要一个匿名 SMB helper。

## 与上游的差异

本仓**不改上游代码**，只在基准上游 tag 之上新增文件：

- `src/bin/smbd.rs`：`smbd --listen <ip:port> --share <名>=<目录>[:ro] …`，匿名共享（只有 guest，不做用户 / 密码）；绑定后向 stdout 打一行 `LISTENING <ip:port>` 并 flush，随后服务到终止信号。
- `.github/workflows/release-windows.yml`：在 `windows-latest` 构建并发布 `smbd-windows-x86_64.exe` 与 `smbd-windows-x86_64.exe.sha256`。
- `README.md` 顶部一段 fork 说明，上游正文原样保留。
- `FORK.md`：本文件。

**本仓不带任何上游补丁。** Windows 共享根在 UNC 路径与映射盘上的枚举 / 建 / 改名 / 删，由上游 `v0.12.1` 起的句柄层解决（`src/fs/windows_dir`、`src/fs/windows_trash`：全部走 `NtCreateFile` 等按父句柄逐名解析，不再经过 cap-std），本仓跟随上游即可。

基准是上游 `v0.12.1`（`c2a38d3`）。

## 同步上游

`main` 上只有两类提交：上游 tag 的原始提交，加一个本仓新增文件的提交。所以同步是"换基"而不是合并：

```sh
git fetch upstream tag vX.Y.Z --no-tags
git switch -c sync/upstream-vX.Y.Z vX.Y.Z
git cherry-pick <本仓新增文件的那个提交>
cargo build --release --bin smbd          # 确认 helper 仍编译
git switch main
git reset --hard sync/upstream-vX.Y.Z
git push --force-with-lease origin main
```

若上游改了 `smbd` 用到的公开 API（`SmbServer::builder` / `Share` / `LocalFsBackend`），改 `src/bin/smbd.rs` 即可。若上游重写了 README 顶部，保留本仓那段 fork 说明、其余取上游正文。

## 发版

发版编号独立于上游（不跟随上游 tag），按 `smbd-v<主>.<次>.<修订>` 递增：

| tag | 基准上游 | 说明 |
| --- | --- | --- |
| `smbd-v0.1.0` | `v0.7.0` | 首个 helper |
| `smbd-v0.2.0` | `v0.9.1` | 跟随上游 |
| `smbd-v0.3.0` | `v0.9.1` | 自带 rooted 目录层（已废弃） |
| `smbd-v0.4.0` | `v0.12.1` | 丢弃自建目录层，跟随上游句柄实现 |

只有 `smbd-v*` 会触发 workflow；上游 `v*` 的 workflow 不会触发。发版：

```sh
git tag smbd-v0.4.0
git push origin main smbd-v0.4.0
```

tag 触发 workflow，Release 里会有 `smbd-windows-x86_64.exe` 与 `smbd-windows-x86_64.exe.sha256`。Solomni 侧用固定地址下载，并校验 `links.yaml` 里的 sha256：

https://github.com/theRMM714/rust-smb-server/releases/download/smbd-v0.4.0/smbd-windows-x86_64.exe
