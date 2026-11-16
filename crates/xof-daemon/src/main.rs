//! xof-daemon: the ButterSat data plane (design §3). Owns the mount, caches,
//! streaming and prefetch; controlled over IPC by the UI and CLI.

mod airports;
mod app;
mod autostart;
mod ipc;
mod prefetch;
mod storage;
mod webapi;
mod xplane;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;
use xof_proto::config::Dirs;
use xof_proto::{Event, LogLine};

#[derive(Parser, Debug)]
#[command(name = "xof-daemon", version, about = "ButterSat scenery streaming daemon")]
struct Args {
    /// Config file (default: per-user config dir).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Do not mount automatically at start.
    #[arg(long)]
    no_mount: bool,
    /// Log filter, e.g. `info` or `xof_core=debug`.
    #[arg(long, default_value = "info")]
    log: String,
}

/// Forwards WARN/ERROR (and INFO from our crates) to IPC subscribers.
struct EventLayer(tokio::sync::broadcast::Sender<Event>);

impl<S: tracing::Subscriber> Layer<S> for EventLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let ours = meta.target().starts_with("xof");
        if *meta.level() > tracing::Level::INFO || (!ours && *meta.level() > tracing::Level::WARN) {
            return;
        }
        struct V(String);
        impl tracing::field::Visit for V {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut v = V(String::new());
        event.record(&mut v);
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let _ = self.0.send(Event::Log(LogLine { level: meta.level().to_string(), message: v.0, unix_time: now }));
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let dirs = Dirs::standard();
    std::fs::create_dir_all(&dirs.data)?;
    std::fs::create_dir_all(&dirs.cache)?;

    // Single instance.
    let lock = std::fs::File::create(dirs.data.join("daemon.lock"))?;
    if lock.try_lock().is_err() {
        eprintln!("xof-daemon is already running");
        std::process::exit(0);
    }

    let (log_tx, _) = tokio::sync::broadcast::channel(512);
    let log_dir = dirs.data.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_file = std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("daemon.log"))?;
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_new(&args.log).unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(log_file)))
        .with(EventLayer(log_tx.clone()))
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("xof-rt").build()?;
    let config_path = args.config.unwrap_or_else(|| dirs.config_file.clone());
    tracing::info!("ButterSat daemon {} starting (config: {})", env!("CARGO_PKG_VERSION"), config_path.display());

    // Switched off (the app's On/Off): don't run, e.g. when started at login.
    if let Ok(cfg) = xof_proto::config::DaemonConfig::load(&config_path) {
        if !cfg.startup.enabled {
            tracing::info!("ButterSat is switched off; exiting (turn it on in the app or with `xof-cli on`)");
            return Ok(());
        }
    }

    let _guard = rt.enter();
    let app = App::new(dirs, config_path, rt.handle().clone())?;
    // Route log events into the app's event stream.
    {
        let events = app.events.clone();
        let mut rx = log_tx.subscribe();
        rt.spawn(async move {
            while let Ok(ev) = rx.recv().await {
                let _ = events.send(ev);
            }
        });
    }

    rt.block_on(run(app, !args.no_mount))
}

use app::App;

async fn run(app: Arc<App>, auto_mount: bool) -> anyhow::Result<()> {
    // Catalog first (so the mount lists streamed tiles), then mount.
    app.clone().refresh_catalog().await;
    if auto_mount && app.config().mount.auto_mount {
        let a = app.clone();
        match tokio::task::spawn_blocking(move || a.mount()).await? {
            Ok(_) => {}
            Err(e) => tracing::warn!("mount failed: {e:#}"),
        }
    }
    tokio::spawn(app.clone().housekeeping());
    tokio::spawn(app.clone().sim_phase());
    tokio::spawn(app.clone().last_position());
    {
        let a = app.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc::serve(a).await {
                tracing::error!("IPC server failed: {e:#}");
            }
        });
    }
    // Periodic events for UIs.
    {
        let a = app.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut n = 0u64;
            loop {
                tick.tick().await;
                if n % 5 == 0 {
                    let h = a.clone();
                    let _ = tokio::task::spawn_blocking(move || h.sample_history()).await;
                }
                n += 1;
                if a.events.receiver_count() == 0 {
                    continue;
                }
                let _ = a.events.send(Event::Stats(Box::new(a.status())));
                if let Some(ac) = a.web.aircraft.borrow().clone() {
                    let _ = a.events.send(Event::Aircraft(ac));
                }
                if n % 30 == 1 {
                    let _ = a.events.send(Event::Health(a.health()));
                }
            }
        });
    }
    let mut shutdown = app.shutdown.subscribe();
    tokio::select! {
        _ = shutdown.changed() => tracing::info!("shutdown requested"),
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
    }
    let a = app.clone();
    tokio::task::spawn_blocking(move || a.unmount()).await?;
    Ok(())
}
