//! Solomni 的 SMB 共享 helper：把若干宿主目录以**匿名**共享暴露在本地 TCP 上。
//! 用法：`smbd --listen <ip:port> --share <名>=<目录>[:ro] [--share ...]`
//! 成功绑定后向 stdout 打一行 `LISTENING <ip:port>` 并 flush，随后服务到终止信号。
//! 约束：只支持匿名共享（guest 用 `mount -t cifs -o guest`）；不做用户/密码。
use smb_server::{LocalFsBackend, Share, SmbServer};
use std::io::Write;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!("用法：smbd --listen <ip:port> --share <名>=<目录>[:ro] [--share ...]");
    std::process::exit(2);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut listen: Option<String> = None;
    let mut shares: Vec<(String, PathBuf, bool)> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--listen" => listen = Some(it.next().unwrap_or_default()),
            "--share" => {
                let spec = it.next().unwrap_or_default();
                let (name, rest) = spec.split_once('=').unwrap_or_else(|| usage());
                let (dir, ro) = match rest.strip_suffix(":ro") {
                    Some(d) => (d, true),
                    None => (rest, false),
                };
                if name.is_empty() || dir.is_empty() {
                    usage();
                }
                shares.push((name.to_string(), PathBuf::from(dir), ro));
            }
            _ => usage(),
        }
    }
    let listen: std::net::SocketAddr = listen.unwrap_or_else(|| usage()).parse()?;
    if shares.is_empty() {
        usage();
    }
    let mut b = SmbServer::builder().listen(listen).netbios_name("SOLOMNI");
    for (name, dir, ro) in &shares {
        let fs = LocalFsBackend::new(dir)?;
        b = b.share(if *ro {
            Share::new(name, fs.read_only()).public_read_only()
        } else {
            Share::new(name, fs).public()
        });
    }
    let server = b.build()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let bound = server.bind().await?;
        println!("LISTENING {}", bound);
        std::io::stdout().flush()?;
        let handle = server.shutdown_handle();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            handle.shutdown();
        });
        server.serve().await
    })?;
    Ok(())
}
