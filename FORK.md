# rust-smb-server（fork）

本仓是 [paltaio/rust-smb-server](https://github.com/paltaio/rust-smb-server) 的 fork。
保留它的目的有两个：

1. **保险**：上游删库、改名或归档时，Solomni 仍有可拉取的源码与可构建的 helper。
2. **预编译 smbd**：上游只发布 crate，不发 Windows 二进制；Solomni 的 vm 档需要一个匿名 SMB helper。

## 与上游的差异

本仓**不动上游代码**，只做两件事：

- 在 `README.md` 顶部加一段 fork 说明，上游正文原样保留；
- 新增文件：
  - `src/bin/smbd.rs`：`smbd --listen <ip:port> --share <名>=<目录>[:ro] …`，匿名共享，绑定后向 stdout 打一行 `LISTENING <ip:port>`；
  - `.github/workflows/release-windows.yml`：在 `windows-latest` 构建并发布 `smbd-windows-x86_64.exe`；
  - `FORK.md`：本文件。

基准是上游 `v0.7.0`（`ac13f30`）。Windows 支持（定位 IO、`unlink` 目录识别、Windows CI）已经在上游，
本仓不再维护任何上游补丁。

## 同步上游

```sh
git remote add upstream https://github.com/paltaio/rust-smb-server.git  # 只需一次
git config --add remote.upstream.tagOpt --no-tags
git fetch upstream
git rebase upstream/main      # main 上只有一个本仓提交
```

如果上游改了 `smbd` 用到的公开 API（`SmbServer::builder` / `Share` / `LocalFsBackend`），
改 `src/bin/smbd.rs` 即可。若上游重写了 README 顶部，保留本仓那段 fork 说明、其余取上游正文。

## 发版

发版编号独立于上游（不跟随上游 tag），从 `smbd-v0.1.0` 开始按自己的节奏递增。
只有 `smbd-v*` 会触发 workflow；上游的 `v*` 发布 workflow 不会被触发。

```sh
git tag smbd-v0.1.0
git push origin main smbd-v0.1.0
```

tag 触发 workflow，Release 里会有：

- `smbd-windows-x86_64.exe`
- `smbd-windows-x86_64.exe.sha256`

稳定下载地址（跟随 latest release）：

https://github.com/theRMM714/rust-smb-server/releases/latest/download/smbd-windows-x86_64.exe
