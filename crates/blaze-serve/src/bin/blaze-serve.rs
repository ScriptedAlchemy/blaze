//! blaze's cache server as a program: `blaze-serve --dir DIR --listen ADDR`. It listens on the
//! address given, loopback unless told otherwise, and has no authentication (see the crate's
//! docs), so a shared one belongs behind what authenticates its clients.

use std::net::SocketAddr;
use std::path::PathBuf;

use blaze_serve::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut dir, mut listen) = (None, None);
    while let Some(arg) = args.next() {
        let value = args.next();
        match (arg.as_str(), value) {
            ("--dir", Some(value)) => dir = Some(PathBuf::from(value)),
            ("--listen", Some(value)) => listen = Some(value.parse::<SocketAddr>()?),
            _ => anyhow::bail!("usage: blaze-serve --dir DIR --listen ADDR (at {arg})"),
        }
    }
    let dir = dir.ok_or_else(|| anyhow::anyhow!("blaze-serve needs --dir"))?;
    let listen = listen.ok_or_else(|| anyhow::anyhow!("blaze-serve needs --listen"))?;
    std::fs::create_dir_all(&dir)?;
    let mut config = Config::at(dir, listen.port());
    config.listen = listen;
    blaze_serve::serve(config).await
}
