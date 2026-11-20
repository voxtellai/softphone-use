//! A scriptable SIP device for testing calls through a PBX. It registers as a
//! device, answers as a person, an AI agent, silence, a tone or an echo, and records levels,
//! audio and the SIP trace. `serve` runs the phone with its control API and a native window;
//! `daemon` runs it without the window; `mcp` exposes the API to an MCP client.

mod ai;
mod api;
mod audio;
mod call;
mod config;
mod g711;
mod human;
mod mcp;
#[cfg(test)]
mod pbx_tests;
mod phone;
mod rtp;
mod sip;
mod store;
mod trace;
mod ui;

use std::sync::Arc;

use config::{Config, DEFAULT_HTTP_ADDR, env_or};
use phone::Phone;

const USAGE: &str = concat!(
    "softphone-use ",
    env!("CARGO_PKG_VERSION"),
    "

usage: softphone-use [serve|daemon|mcp|version] [rune flags]

  serve   register, serve the API and open the window (default)
  daemon  register and serve the API, without a window
  mcp     MCP over stdio, as a client of a running serve or daemon
  version print the version

Configuration comes from the environment and from .env.local or .env in the
working directory. See https://github.com/voxtellai/softphone-use"
);

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(flag @ ("--version" | "-V" | "--help" | "-h")) = args.first().map(String::as_str) {
        args[0] = if flag.contains('V') || flag == "--version" { "version".into() } else { "help".into() };
    }
    let cmd = if args.first().is_some_and(|a| !a.starts_with('-')) { args.remove(0) } else { "serve".into() };
    let result = match cmd.as_str() {
        "serve" => serve(args, true),
        "daemon" => serve(args, false),
        "mcp" => {
            config::load_env_files();
            runtime().block_on(mcp::run(format!("http://{}", env_or("SOFTPHONE_HTTP_ADDR", DEFAULT_HTTP_ADDR))))
        }
        "version" => {
            println!("softphone-use {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" => {
            println!("{USAGE}");
            Ok(())
        }
        _ if matches!(cmd.as_str(), "-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(anyhow::anyhow!("unknown command {other:?} (want serve, daemon or mcp)")),
    };
    if let Err(e) = result {
        eprintln!("softphone-use: {e:#}");
        std::process::exit(1);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime")
}

fn serve(args: Vec<String>, window: bool) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    // Rune's headless tooling (--snap, --tree, --script) renders the UI with demo data and no phone.
    if window && ui::is_tooling(&args) {
        ui::run_demo(args);
        return Ok(());
    }

    let cfg = Config::load()?;
    let rt = runtime();
    let phone = Phone::new(cfg.clone(), rt.handle().clone());
    let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);

    let listener = rt.block_on(tokio::net::TcpListener::bind(&cfg.http_addr))?;
    tracing::info!(url = %format!("http://{}", cfg.http_addr), "Control API");
    let app = api::router(phone.clone());
    let mut http_shutdown = shutdown.clone();
    rt.spawn(async move {
        let stop = async move {
            let _ = http_shutdown.wait_for(|v| *v).await;
        };
        if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(stop).await {
            tracing::error!(error = %e, "HTTP server failed");
        }
    });

    let phone_task = rt.spawn(phone.clone().run(shutdown.clone()));
    {
        let tx = shutdown_tx.clone();
        rt.spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            tx.send_replace(true);
        });
    }

    if window {
        let rt_handle = rt.handle().clone();
        let watcher = shutdown.clone();
        ui::run(Arc::clone(&phone), args, watcher);
        shutdown_tx.send_replace(true);
        let _ = rt_handle;
    }
    let result = rt.block_on(phone_task)?;
    rt.shutdown_timeout(std::time::Duration::from_secs(3));
    result
}
