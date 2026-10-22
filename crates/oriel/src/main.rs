//! `oriel`: one binary that hosts a desktop and connects to other hosts.

mod config;

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use oriel_client::{MouseMode, ViewerOptions, keymap::Shortcut};
use oriel_net::{Identity, Paths, TrustStore};
use oriel_proto::{Codec, DEFAULT_PORT, StreamSettings};

#[derive(Parser)]
#[command(version, about = "Low-latency remote desktop: host and client in one")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Also write logs to this file.
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    H264,
    Hevc,
    Av1,
}

impl From<CodecArg> for Codec {
    fn from(c: CodecArg) -> Self {
        match c {
            CodecArg::H264 => Codec::H264,
            CodecArg::Hevc => Codec::Hevc,
            CodecArg::Av1 => Codec::Av1,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum MouseArg {
    Auto,
    Desktop,
    Captured,
}

#[derive(Subcommand)]
enum Command {
    /// Share this computer's display.
    Host {
        #[arg(long, default_value_t = SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)))]
        listen: SocketAddr,
        /// Name shown to clients (default: the computer's name).
        #[arg(long)]
        name: Option<String>,
        /// Stream a synthetic test pattern instead of the real display.
        #[arg(long)]
        test_pattern: bool,
        /// Accept pairing requests; prints a PIN to enter on the client.
        #[arg(long)]
        pair: bool,
        /// Use this PIN instead of a random one (implies --pair).
        #[arg(long)]
        pin: Option<String>,
        /// Force an FFmpeg encoder, e.g. hevc_nvenc.
        #[arg(long)]
        encoder: Option<String>,
        /// Only offer this codec.
        #[arg(long, value_enum)]
        codec: Option<CodecArg>,
        /// Bitrate when the client does not ask for one (kbit/s).
        #[arg(long, default_value_t = 30_000)]
        bitrate: u32,
    },
    /// Pair with a host that runs `oriel host --pair`.
    Pair {
        /// Host address (name or IP, optionally with :port).
        address: String,
        /// The PIN the host printed.
        #[arg(long)]
        pin: String,
        /// Name to save the host under (default: the host's own name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Connect to a paired host.
    Connect {
        /// Host name (as listed by `oriel hosts`) or address.
        host: String,
        #[arg(long)]
        fps: Option<u32>,
        /// Bitrate in kbit/s.
        #[arg(long)]
        bitrate: Option<u32>,
        /// Preferred codecs, most preferred first.
        #[arg(long, value_enum, value_delimiter = ',')]
        codec: Vec<CodecArg>,
        /// Decode on the CPU.
        #[arg(long)]
        software_decode: bool,
        #[arg(long)]
        fullscreen: bool,
        #[arg(long, value_enum)]
        mouse: Option<MouseArg>,
        /// Show the statistics overlay.
        #[arg(long)]
        stats: bool,
        /// Shortcut that opens the menu and releases the keyboard.
        #[arg(long)]
        menu_shortcut: Option<String>,
        /// Stream without a window for this many seconds, printing stats.
        #[arg(long)]
        headless: Option<u64>,
        /// Save a screenshot of the window after the first frame, then exit.
        #[arg(long)]
        screenshot: Option<PathBuf>,
        #[arg(long, default_value_t = 2.0)]
        screenshot_after: f64,
    },
    /// List paired hosts.
    Hosts,
    /// Forget a paired host.
    Forget { name: String },
    /// List clients paired with this host.
    Clients,
    /// Remove a paired client.
    Unpair { name: String },
    /// Show this device's identity and media capabilities.
    Info,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;
    let default_log = matches!(cli.command, Command::Host { .. })
        .then(|| cfg!(windows).then(|| paths.config_dir.join("oriel-host.log")))
        .flatten();
    init_logging(cli.log_file.clone().or(default_log))?;
    oriel_media::init_logging();

    match cli.command {
        Command::Host {
            listen,
            name,
            test_pattern,
            pair,
            pin,
            encoder,
            codec,
            bitrate,
        } => {
            let pair_pin = match (pair, pin) {
                (_, Some(pin)) => Some(pin),
                (true, None) => Some(oriel_net::pairing::generate_pin()),
                (false, None) => None,
            };
            let options = oriel_host::HostOptions {
                listen,
                name: name.unwrap_or_else(oriel_client::connection::device_name),
                source: if test_pattern {
                    oriel_host::SourceKind::TestPattern
                } else {
                    oriel_host::SourceKind::Auto
                },
                pair_pin,
                encoder,
                codec: codec.map(Into::into),
                default_bitrate_kbps: bitrate,
            };
            runtime()?.block_on(oriel_host::run(paths, options))
        }
        Command::Pair { address, pin, name } => {
            let name = runtime()?.block_on(oriel_client::connection::pair(
                &paths,
                &address,
                &pin,
                name.as_deref(),
            ))?;
            println!("Paired with {name}. Connect with: oriel connect {name}");
            Ok(())
        }
        Command::Connect {
            host,
            fps,
            bitrate,
            codec,
            software_decode,
            fullscreen,
            mouse,
            stats,
            menu_shortcut,
            headless,
            screenshot,
            screenshot_after,
        } => {
            let file = config::ClientConfig::load(&paths)?;
            let codecs: Vec<Codec> = if codec.is_empty() {
                file.codecs()?
            } else {
                codec.into_iter().map(Into::into).collect()
            };
            let settings = StreamSettings {
                display: None,
                codecs,
                bitrate_kbps: bitrate.unwrap_or(file.bitrate_kbps),
                max_fps: fps.unwrap_or(file.max_fps),
            };
            let rt = runtime()?;
            if let Some(secs) = headless {
                return rt.block_on(oriel_client::headless::run(
                    &paths,
                    &host,
                    settings,
                    !software_decode,
                    Duration::from_secs(secs),
                ));
            }
            let shortcut_text = menu_shortcut.unwrap_or(file.menu_shortcut.clone());
            let menu_shortcut = Shortcut::parse(&shortcut_text)
                .with_context(|| format!("cannot parse shortcut {shortcut_text:?}"))?;
            let mouse_mode = match mouse {
                Some(MouseArg::Auto) => MouseMode::Auto,
                Some(MouseArg::Desktop) => MouseMode::Absolute,
                Some(MouseArg::Captured) => MouseMode::Relative,
                None => file.mouse_mode()?,
            };
            oriel_client::app::run(
                rt,
                paths,
                ViewerOptions {
                    target: host,
                    settings,
                    hardware_decode: !software_decode,
                    menu_shortcut,
                    mouse_mode,
                    fullscreen,
                    show_stats: stats,
                    screenshot: screenshot.map(|p| (p, Duration::from_secs_f64(screenshot_after))),
                },
            )
        }
        Command::Hosts => {
            let store = TrustStore::load(&paths.known_hosts())?;
            if store.peers.is_empty() {
                println!("No paired hosts. Pair with: oriel pair <address> --pin <PIN>");
            }
            for p in store.peers {
                println!(
                    "{:<20} {:<28} {}",
                    p.name,
                    p.address.unwrap_or_default(),
                    p.fingerprint.short()
                );
            }
            Ok(())
        }
        Command::Forget { name } => {
            let mut store = TrustStore::load(&paths.known_hosts())?;
            if !store.remove(&name) {
                bail!("no paired host named {name}");
            }
            store.save(&paths.known_hosts())
        }
        Command::Clients => {
            let store = TrustStore::load(&paths.trusted_clients())?;
            if store.peers.is_empty() {
                println!("No paired clients. Start the host with --pair to pair one.");
            }
            for p in store.peers {
                println!("{:<20} {}", p.name, p.fingerprint.short());
            }
            Ok(())
        }
        Command::Unpair { name } => {
            let mut store = TrustStore::load(&paths.trusted_clients())?;
            if !store.remove(&name) {
                bail!("no paired client named {name}");
            }
            store.save(&paths.trusted_clients())
        }
        Command::Info => {
            let identity = Identity::load_or_create(&paths.config_dir)?;
            println!("config:      {}", paths.config_dir.display());
            println!("fingerprint: {}", identity.fingerprint);
            println!(
                "encoders:    {}",
                oriel_media::available_encoders().join(", ")
            );
            oriel_host::init_process();
            for d in oriel_host::capture::list_displays(oriel_host::SourceKind::Auto) {
                println!(
                    "display {}:   {} {}x{} @ {} Hz{}",
                    d.id,
                    d.name,
                    d.width,
                    d.height,
                    d.refresh_hz,
                    if d.primary { " (primary)" } else { "" }
                );
            }
            Ok(())
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?)
}

fn init_logging(file: Option<PathBuf>) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
    let filter = EnvFilter::try_from_env("ORIEL_LOG").unwrap_or_else(|_| {
        EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn,naga=warn,quinn=warn,egui=warn")
    });
    let file_layer = match file {
        Some(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open log file {}", path.display()))?;
            Some(
                fmt::layer()
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(f)),
            )
        }
        None => None,
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        .init();
    Ok(())
}
