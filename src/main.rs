mod check;
mod config;
mod engine;
mod metadata;
mod runner;
mod state;
mod telegram;

use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use engine::{Engine, Event, FsAction};
use notify::Watcher;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);
    let mut cfg = config::Config::load(config_path.as_ref())?;
    cfg.expanded();

    std::fs::create_dir_all(&cfg.checks_dir)
        .with_context(|| format!("cannot create checks dir {}", cfg.checks_dir.display()))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();

    let mut engine = Engine::new(&cfg, tx.clone())?;
    engine.set_states(engine_states(&cfg)?);
    engine.scan_dir(&cfg.checks_dir);

    // filesystem watcher: std channel -> blocking thread -> tokio channel
    let (fs_tx, fs_rx_std) = std::sync::mpsc::channel();
    let mut _watcher = notify::recommended_watcher(move |res| {
        let _ = fs_tx.send(res);
    })?;
    _watcher.watch(&cfg.checks_dir, notify::RecursiveMode::NonRecursive)?;
    tracing::info!("watching {}", cfg.checks_dir.display());
    let fs_tx_tokio = tx.clone();
    tokio::task::spawn_blocking(move || {
        while let Ok(res) = fs_rx_std.recv() {
            let action = match res {
                Ok(event) => match event.kind {
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {
                        FsAction::Upsert(event.paths.into_iter().next().unwrap_or_default())
                    }
                    notify::EventKind::Remove(_) => {
                        FsAction::Remove(event.paths.into_iter().next().unwrap_or_default())
                    }
                    _ => continue,
                },
                Err(e) => {
                    tracing::error!("fs watch error: {e}");
                    continue;
                }
            };
            let _ = fs_tx_tokio.send(Event::Fs(action));
        }
    });

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = ticker.tick() => engine.run_due(),
            ev = rx.recv() => match ev {
                Some(Event::Fs(action)) => engine.handle_fs(action),
                Some(Event::FsRecheck(path)) => engine.handle_recheck(&path),
                Some(Event::Watch { path, add }) => {
                    let res = if add {
                        _watcher.watch(&path, notify::RecursiveMode::NonRecursive)
                    } else {
                        _watcher.unwatch(&path)
                    };
                    if let Err(e) = res {
                        tracing::warn!("cannot {}watch {}: {e}", if add { "" } else { "un" }, path.display());
                    }
                }
                Some(Event::Done { id, gen, outcome }) => engine.handle_done(&id, gen, outcome),
                None => break,
            },
        }
    }
    Ok(())
}

fn engine_states(cfg: &config::Config) -> Result<state::States> {
    state::StateStore::new(cfg.state_dir()).load_all()
}
