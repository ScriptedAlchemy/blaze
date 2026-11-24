//! `blaze-serve` CLI binary: `blaze-serve --dir DIR --listen ADDR`.
//!
//! Listens on the requested TCP address (loopback unless configured otherwise) with no
//! authentication. Uses `blaze-serve`'s `#![no_std]` driver and `blaze_own::Heap::process()`.

use core::fmt;
use core::net::SocketAddr;

use blaze_own::{Heap, format_in};
use blaze_serve::{Config, process_args, serve, write_stderr};

/// Marker error returning a non-zero exit status from `main` after printing to stderr.
struct CliExit;

impl fmt::Debug for CliExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("blaze-serve exited with error")
    }
}

fn main() -> Result<(), CliExit> {
    let heap = Heap::process();
    let alloc = heap.shared();

    let args = match process_args(alloc.clone()) {
        Ok(args) => args,
        Err(err) => {
            let msg = format_in!(alloc, "blaze-serve: reading argv failed: {}\n", err.what());
            write_stderr(msg.as_bytes());
            return Err(CliExit);
        }
    };

    let mut dir: Option<&str> = None;
    let mut listen: Option<SocketAddr> = None;
    let mut idx = 1usize;
    while let Some(flag) = args.get(idx) {
        let next = args.get(idx.saturating_add(1));
        match (flag.as_str(), next) {
            ("--dir", Some(value)) => dir = Some(value.as_str()),
            ("--listen", Some(value)) => match value.as_str().parse::<SocketAddr>() {
                Ok(addr) => listen = Some(addr),
                Err(_) => {
                    let msg = format_in!(
                        alloc,
                        "blaze-serve: invalid --listen address {:?}\n",
                        value.as_str()
                    );
                    write_stderr(msg.as_bytes());
                    return Err(CliExit);
                }
            },
            _ => {
                let msg = format_in!(
                    alloc,
                    "usage: blaze-serve --dir DIR --listen ADDR (unexpected {:?})\n",
                    flag.as_str()
                );
                write_stderr(msg.as_bytes());
                return Err(CliExit);
            }
        }
        idx = idx.saturating_add(2);
    }

    let Some(dir) = dir else {
        write_stderr(b"blaze-serve: missing required --dir DIR\n");
        return Err(CliExit);
    };
    let Some(listen) = listen else {
        write_stderr(b"blaze-serve: missing required --listen ADDR\n");
        return Err(CliExit);
    };

    let mut config = Config::at(dir, listen.port(), alloc.clone());
    config.listen = listen;
    if let Err(err) = serve(config, alloc.clone()) {
        let msg = format_in!(alloc, "blaze-serve: {err}\n");
        write_stderr(msg.as_bytes());
        return Err(CliExit);
    }
    Ok(())
}
