# rust-smb-server（fork）

本仓是 [paltaio/rust-smb-server](https://github.com/paltaio/rust-smb-server) 的 fork。
保留它的目的有两个：

1. **保险**：上游删库、改名或归档时，Solomni 仍有可拉取的源码与可构建的 helper。
2. **预编译 smbd**：上游只发布 crate，不发 Windows 二进制；Solomni 的 vm 档需要一个匿名 SMB helper。

## 与上游的差异

策略是**尽量与上游同步，按需维护最小补丁集**：main 上只有下面两组提交，逐版本 rebase 到
`upstream/main`；上游自己解决了同类问题时，把对应补丁删掉。

### 1. fork 自有的构建与说明文件

- `README.md` 顶部加一段 fork 说明，上游正文原样保留；
- `src/bin/smbd.rs`：`smbd --listen <ip:port> --share <名>=<目录>[:ro] …`，匿名共享，绑定后向
  stdout 打一行 `LISTENING <ip:port>`；
- `.github/workflows/release-windows.yml`：在 `windows-latest` 构建并发布
  `smbd-windows-x86_64.exe`；
- `FORK.md`：本文件。

### 2. 用 `src/fs/rooted` 取代 `cap-std`

**为什么需要**：`cap-primitives` 在 Windows 上把句柄的最终路径当作可重开的路径字符串用。
`src/windows/fs/get_path.rs` 只剥 4 个字符的 `\\?\` 前缀，于是共享根句柄的
`\\?\UNC\server\share\…` 变成相对路径 `UNC\server\share\…`，再交给 `std::fs`
（按进程当前目录解析）。结果是网络共享上的 `read_dir` / `create_dir` / `remove_dir` /
`remove_file` / `rename` / `hard_link` / `read_link` / `set_permissions` / `symlink`
必然失败，而 `open` / `read` / `write` / `metadata`（走句柄）正常——真机上表现为枚举与建目录
返回 `STATUS_OBJECT_NAME_NOT_FOUND`、删除返回成功但文件还在。上游 #318、#348 是同一问题；
`cap-std` 已移出 Bytecode Alliance、只做最小维护。

**补丁内容**（约 1.5k 行）：

- 新增 `src/fs/rooted/`：`Dir`（持有已打开目录句柄）、`File`、`OpenOptions`、
  `Metadata`、`FileType`、`Permissions`、`DirEntry` / `Entries`；
  - `unix.rs`：`openat` / `statat` / `statx` / `readlinkat` / `mkdirat` / `unlinkat` /
    `renameat` / `RawDir`；
  - `windows.rs` 与 `nt.rs`：`NtCreateFile`（`RootDirectory` 相对名字）、
    `NtQueryDirectoryFile`、`NtSetInformationFile`（`FileRenameInformation`）、
    `FileDispositionInfoEx`、`GetFileInformationByHandle`；
  - 所有路径**逐名解析**，不把句柄回写成路径字符串，因此本地盘与网络共享走同一条代码路径；
  - 符号链接、junction 的目标必须是根内的相对路径，绝对目标与越界 `..` 一律拒绝，链路超过 40 段报
    `ELOOP`；`..` 用已打开句柄栈回退，栈空即逃逸；
- `Cargo.toml` 删掉 `cap-std` / `cap-primitives`，`windows-sys` 增加
  `Win32_System_Ioctl`（`FSCTL_GET_REPARSE_POINT`）；
- `src/fs/local.rs`、`windows_streams`、`windows_eas`、`open_streams`、`volume`、
  `macos_streams`、`streams` 的调用点换成 `crate::fs::rooted`；
- 新增 `src/fs/rooted/tests.rs`：创建、写入、枚举、改名、删除（含「句柄仍打开时名字立即消失」）、
  越界拒绝、unix 符号链接、Windows junction 逃逸拒绝，以及 `\\localhost\<盘符>$` 上的网络共享用例
  （共享不可达时跳过）。

### 行为差异（相对 `cap-std`）

1. API 面按本仓的实际调用裁剪，只在用到的平台提供：`unix.rs` 有 `open` / `open_dir`，
   Linux 另有 `from_std_file` / `remove_dir_all`，`windows.rs` 没有它们；`OpenOptions` 只有
   `read` / `write` / `truncate` / `create_new` / `custom_flags`，Windows 另有
   `access_mode`，没有 `append` / `create` / `follow`。上游新版本用到被裁掉的方法时会在编译期
   报错，按需补回即可。
2. Windows 不再特判 DOS 设备名（`CON` / `PRN` / `AUX` / `NUL` / `COM0-9` / `LPT0-9`）。
   `cap-std` 对这些名字直接返回 `ERROR_FILE_NOT_FOUND`；本层把它们当根内的普通名字——相对
   句柄打开不会落到设备对象上，行为与 unix 一致。
3. Windows 的删除先用 `FileDispositionInfoEx` 加 `FILE_DISPOSITION_FLAG_POSIX_SEMANTICS`：
   名字立刻消失，另一个句柄仍打开也不影响，和 Unix、SMB 客户端看到的一致。共享（网络重定向器）
   对这套 flag 可能回 `ERROR_INVALID_PARAMETER` / `ERROR_NOT_SUPPORTED`，这时退回
   `FileDispositionInfo` 的 delete-on-close（句柄关闭时完成删除）。`cap-std` 走
   `DeleteFileW` / `RemoveDirectoryW`，被别处打开的目录会留到最后一个句柄关闭；不用
   `FILE_DELETE_ON_CLOSE` 是因为它会吞掉删除失败（例如目录非空），上游
   `unlink_file_then_nonempty_dir_errors` 依赖这个错误。
4. `Metadata`、`Permissions`、`FileType` 是本层的自有类型，时间字段直接是
   `std::time::SystemTime`（`cap-std` 有自己的包装类型，调用方需要 `into_std()`）。
5. 枚举用 `NtQueryDirectoryFile`（`FileFullDirectoryInformation`），条目的类型直接来自
   枚举记录里的属性；`DirEntry::metadata` 用父目录句柄加名字取，不再把条目拼成路径。

## 同步上游

```sh
git remote add upstream https://github.com/paltaio/rust-smb-server.git  # 只需一次
git config --add remote.upstream.tagOpt --no-tags
git fetch upstream --tags
git rebase upstream/main      # main 上只有本仓的补丁提交
```

rebase 时的注意点：

- `Cargo.toml` 的 `[dependencies]`、`workspace.dependencies` 与 `localfs` feature 可能和本仓
  删 `cap-std` 的改动冲突，取上游内容后**再把 `cap-std` / `cap-primitives` 删掉**；
- 上游若新增 `cap_std` / `cap_primitives` 调用，改到 `crate::fs::rooted`，缺少的方法按需补；
- 上游若改了 `smbd` 用到的公开 API（`SmbServer::builder` / `Share` / `LocalFsBackend`），
  改 `src/bin/smbd.rs` 即可；
- 若上游重写了 README 顶部，保留本仓那段 fork 说明、其余取上游正文；
- 上游一旦自己修掉句柄回写路径的问题，删掉第 2 组提交。

## 验证

```sh
cargo test --workspace --all-features
```

Windows 上 `rooted::tests::windows::works_through_a_network_share` 会找本机的
`\\localhost\<盘符>$` 共享做端到端验证；CI 里没有可用的共享，该用例自行跳过。

## 发版

发版编号独立于上游（不跟随上游 tag），从 `smbd-v0.1.0` 起按自己的节奏递增。
只有 `smbd-v*` 会触发 workflow；上游的 `v*` 发布 workflow 不会被触发。

`smbd-v0.1.0` 基于上游 v0.7.0，`smbd-v0.2.0` 起基于带 rooted 层的 v0.9.1。

```sh
git tag smbd-v0.2.0
git push origin main smbd-v0.2.0
```

tag 触发 workflow，Release 里会有：

- `smbd-windows-x86_64.exe`
- `smbd-windows-x86_64.exe.sha256`

稳定下载地址（跟随 latest release）：

https://github.com/theRMM714/rust-smb-server/releases/latest/download/smbd-windows-x86_64.exe
