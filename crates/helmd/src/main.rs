//! helmd binary.
//!
//!   helmd serve [--socket PATH]   run the daemon in the foreground
//!   helmd stdio [--socket PATH]   bridge stdio ⇄ the daemon socket,
//!                                 spawning `serve` if absent — what helm
//!                                 runs over an SSH exec channel
//!   helmd --version               print version + protocol version

use std::path::PathBuf;

fn socket_arg(args: &[String]) -> PathBuf {
    args.iter()
        .position(|a| a == "--socket")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(helmd::server::default_socket_path)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!(
                "helmd {} (proto {})",
                env!("CARGO_PKG_VERSION"),
                helm_proto::PROTOCOL_VERSION
            );
            Ok(())
        }
        Some("serve") => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .with_writer(std::io::stderr)
                .init();
            let socket = socket_arg(&args);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(helmd::server::serve(&socket))
        }
        Some("stdio") => {
            // No stdout logging — stdout IS the protocol stream.
            let socket = socket_arg(&args);
            helmd::server::stdio_bridge(&socket)
        }
        _ => {
            eprintln!("usage: helmd serve|stdio [--socket PATH] | helmd --version");
            std::process::exit(2);
        }
    }
}
