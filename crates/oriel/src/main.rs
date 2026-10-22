//! `oriel`: one binary that hosts a desktop and connects to other hosts.

// No console window when started from Explorer; commands typed in a
// terminal attach to it (see `attach_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod update;

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use oriel_client::{
    MouseMode, ViewerOptions,
    keymap::{Command as ViewerCommand, Shortcut},
};
use oriel_net::{Identity, Paths, TrustStore};
use oriel_proto::{Codec, DEFAULT_PORT, StreamSettings};

#[derive(Parser)]
#[command(version, about = "Low-latency remote desktop: host and client in one")]
struct Cli {
    /// Without a command, opens the launcher window.
    #[command(subcommand)]
    command: Option<Command>,
    /// Also write logs to this file.
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    H264,
    Hevc,
    Av1,
    /// Intra-only wavelet codec for fast LANs (hundreds of Mbit/s).
    Pyrowave,
    /// PyroWave when the link allows, otherwise HEVC, then H.264.
    Auto,
}

impl From<CodecArg> for Codec {
    fn from(c: CodecArg) -> Self {
        match c {
            CodecArg::H264 => Codec::H264,
            CodecArg::Hevc => Codec::Hevc,
            CodecArg::Av1 => Codec::Av1,
            CodecArg::Pyrowave => Codec::PyroWave,
            CodecArg::Auto => unreachable!("expanded by codec_list"),
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
        /// Development: exit with code 75 when this file changes (a wrapper
        /// script then installs the new build and restarts).
        #[arg(long, hide = true)]
        restart_when_changed: Option<PathBuf>,
        /// Development: install new builds that appear in this folder and
        /// restart into them.
        #[arg(long, hide = true)]
        update_from: Option<PathBuf>,
        /// Do not show a tray icon (Windows).
        #[arg(long)]
        no_tray: bool,
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
        /// Preferred codecs, most preferred first; `auto` switches between
        /// PyroWave and HEVC/H.264 depending on the link.
        #[arg(long, value_enum, value_delimiter = ',')]
        codec: Vec<CodecArg>,
        /// PyroWave bitrate in kbit/s.
        #[arg(long)]
        pyrowave_bitrate: Option<u32>,
        /// Decode on the CPU.
        #[arg(long)]
        software_decode: bool,
        /// Do not stream sound.
        #[arg(long)]
        no_audio: bool,
        /// Display to stream (see `oriel info` on the host); default primary.
        #[arg(long)]
        display: Option<u32>,
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
    /// Run the host as a Windows service (lock screen, UAC, login screen).
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Install to Program Files and start the service (as Administrator).
    /// Arguments after `--` are passed to `oriel host`.
    Install {
        #[arg(last = true)]
        host_args: Vec<String>,
    },
    /// Stop and remove the service (as Administrator).
    Uninstall,
    /// Print a PIN that pairs a client with the service host within ten
    /// minutes (as Administrator).
    Pair,
    /// Entry point used by the service manager.
    #[command(hide = true)]
    Run {
        #[arg(last = true)]
        host_args: Vec<String>,
    },
}

fn main() -> Result<()> {
    #[cfg(windows)]
    attach_console();
    let result = run();
    if let Err(e) = &result {
        // GUI-subsystem processes have nowhere to print; make sure the log
        // file has it.
        tracing::error!("{e:#}");
    }
    result
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    #[cfg(windows)]
    if matches!(
        cli.command,
        Some(Command::Service(ServiceCommand::Run { .. }))
    ) {
        // The service and the host it starts share the service's data
        // directory, not SYSTEM's profile.
        // SAFETY: still single-threaded.
        unsafe { std::env::set_var("ORIEL_CONFIG_DIR", oriel_host::service::data_dir()) };
    }
    let paths = Paths::discover()?;
    let default_log = match cli.command {
        Some(Command::Host { .. }) if cfg!(windows) => {
            Some(paths.config_dir.join("oriel-host.log"))
        }
        Some(Command::Service(ServiceCommand::Run { .. })) => {
            Some(paths.config_dir.join("oriel-service.log"))
        }
        _ => None,
    };
    init_logging(cli.log_file.clone().or(default_log))?;
    oriel_media::init_logging();

    let Some(command) = cli.command else {
        let config = oriel_client::config::ClientConfig::load(&paths).unwrap_or_default();
        let log_path = cli.log_file.clone();
        return oriel_client::launcher::run(
            runtime()?,
            paths.clone(),
            Some(Box::new(LocalHost::new(
                paths,
                LocalHost::default_options(None),
            )?)),
            oriel_client::launcher::GuiOptions {
                start_sharing: config.share,
                log_path,
                ..Default::default()
            },
        );
    };
    match command {
        Command::Host {
            listen,
            name,
            test_pattern,
            pair,
            pin,
            encoder,
            codec,
            bitrate,
            restart_when_changed,
            update_from,
            no_tray,
        } => {
            update::clean_old();
            if let Some(path) = restart_when_changed {
                // Older dev scripts loop on exit code 75; hand over to a
                // tray host that updates itself instead.
                let dir = path.parent().map(PathBuf::from).unwrap_or_default();
                let args: Vec<String> = std::env::args().skip(1).collect();
                let mut out = Vec::new();
                let mut skip = false;
                for a in args {
                    if skip {
                        skip = false;
                        continue;
                    }
                    if a == "--restart-when-changed" {
                        skip = true;
                        continue;
                    }
                    out.push(a);
                }
                out.push("--update-from".into());
                out.push(dir.to_string_lossy().into_owned());
                std::process::Command::new(std::env::current_exe()?)
                    .args(&out)
                    .spawn()?;
                return Ok(());
            }
            let log_path = cli
                .log_file
                .clone()
                .or_else(|| cfg!(windows).then(|| paths.config_dir.join("oriel-host.log")));
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
            let tray = cfg!(windows) && !no_tray;
            let restart = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            if let Some(src) = update_from {
                let flag = restart.clone();
                update::watch(src, move || {
                    if tray {
                        // Let the app close its window and tray icon first.
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        update::relaunch();
                    }
                });
            }
            if tray {
                // The app in the tray, sharing; the window opens from there.
                let result = oriel_client::launcher::run(
                    runtime()?,
                    paths.clone(),
                    Some(Box::new(LocalHost::new(paths, options)?)),
                    oriel_client::launcher::GuiOptions {
                        start_hidden: true,
                        start_sharing: true,
                        log_path,
                        quit_flag: Some(restart.clone()),
                        ..Default::default()
                    },
                );
                if restart.load(std::sync::atomic::Ordering::Relaxed) {
                    update::relaunch();
                }
                return result;
            }
            let _ = log_path;
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
            pyrowave_bitrate,
            software_decode,
            no_audio,
            display,
            fullscreen,
            mouse,
            stats,
            menu_shortcut,
            headless,
            screenshot,
            screenshot_after,
        } => {
            let file = oriel_client::config::ClientConfig::load(&paths)?;
            let (codecs, auto_codec) = if codec.is_empty() {
                file.codecs()?
            } else if codec.iter().any(|c| matches!(c, CodecArg::Auto)) {
                (oriel_client::config::AUTO_CODECS.to_vec(), true)
            } else {
                (codec.into_iter().map(Into::into).collect(), false)
            };
            let settings = StreamSettings {
                display,
                codecs,
                bitrate_kbps: bitrate.unwrap_or(file.bitrate_kbps),
                max_fps: fps.unwrap_or(file.max_fps),
                audio: file.audio && !no_audio,
                auto_codec,
                pyrowave_kbps: pyrowave_bitrate.unwrap_or(file.pyrowave_kbps),
                resolution: None,
            };
            let codec_key = if auto_codec {
                "auto".to_string()
            } else {
                match settings.codecs.first() {
                    Some(Codec::H264) => "h264",
                    Some(Codec::Av1) => "av1",
                    Some(Codec::PyroWave) => "pyrowave",
                    _ => "hevc",
                }
                .to_string()
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
            let mut shortcuts = file.bindings();
            if let Some(text) = menu_shortcut {
                let shortcut = Shortcut::parse(&text)
                    .with_context(|| format!("cannot parse shortcut {text:?}"))?;
                shortcuts.set(ViewerCommand::Menu, Some(shortcut));
            }
            let mouse_mode = match mouse {
                Some(MouseArg::Auto) => MouseMode::Auto,
                Some(MouseArg::Desktop) => MouseMode::Absolute,
                Some(MouseArg::Captured) => MouseMode::Relative,
                None => file.mouse_mode()?,
            };
            let target = host.clone();
            oriel_client::launcher::run(
                rt,
                paths,
                None,
                oriel_client::launcher::GuiOptions {
                    connect: Some(target),
                    viewer: Some(ViewerOptions {
                        target: host,
                        settings,
                        hardware_decode: !software_decode,
                        shortcuts,
                        mouse_mode,
                        fullscreen,
                        show_stats: stats,
                        codec_key,
                        screenshot: screenshot
                            .map(|p| (p, Duration::from_secs_f64(screenshot_after))),
                    }),
                    quit_with_viewer: true,
                    tray: false,
                    ..Default::default()
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
        Command::Service(cmd) => service(cmd),
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
        EnvFilter::new(
            "info,wgpu_core=warn,wgpu_hal=warn,naga=warn,quinn=warn,egui=warn,arboard=error,egui_winit=error,sctk_adwaita=off",
        )
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

#[cfg(windows)]
fn service(cmd: ServiceCommand) -> Result<()> {
    use oriel_host::service;
    match cmd {
        ServiceCommand::Install { host_args } => service::install(&host_args),
        ServiceCommand::Uninstall => service::uninstall(),
        ServiceCommand::Pair => {
            let pin = service::allow_pairing()?;
            println!(
                "Pairing allowed for ten minutes. On the client run:\n  oriel pair <this-host-address> --pin {pin}"
            );
            Ok(())
        }
        ServiceCommand::Run { .. } => service::run_dispatcher(),
    }
}

#[cfg(not(windows))]
fn service(_cmd: ServiceCommand) -> Result<()> {
    bail!("the host service is only available on Windows; run `oriel host` instead")
}

/// Hosting inside the app ("Share this computer" and the tray).
struct LocalHost {
    paths: Paths,
    options: oriel_host::HostOptions,
    rt: tokio::runtime::Runtime,
    control: oriel_host::HostControl,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl LocalHost {
    fn new(paths: Paths, options: oriel_host::HostOptions) -> Result<Self> {
        Ok(Self {
            control: oriel_host::HostControl::new(options.pair_pin.clone()),
            paths,
            options,
            rt: runtime()?,
            task: None,
        })
    }

    fn default_options(pin: Option<String>) -> oriel_host::HostOptions {
        oriel_host::HostOptions {
            listen: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
            name: oriel_client::connection::device_name(),
            source: if oriel_host::native_hosting_available() {
                oriel_host::SourceKind::Auto
            } else {
                oriel_host::SourceKind::TestPattern
            },
            pair_pin: pin,
            encoder: None,
            codec: None,
            default_bitrate_kbps: 30_000,
        }
    }
}

impl oriel_client::launcher::HostControl for LocalHost {
    fn start(&mut self) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        let (paths, options, control) = (
            self.paths.clone(),
            self.options.clone(),
            self.control.clone(),
        );
        self.task = Some(self.rt.spawn(async move {
            if let Err(e) = oriel_host::run_with(paths, options, control).await {
                tracing::error!("host stopped: {e:#}");
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.control.stop_pairing();
    }

    fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }

    fn description(&self) -> String {
        if self.options.source == oriel_host::SourceKind::TestPattern {
            return "Hosting needs Windows or a GNOME session; this shares a test pattern.".into();
        }
        let list: Vec<String> = oriel_host::capture::list_displays(self.options.source)
            .iter()
            .map(|d| format!("{} ({}×{})", d.name, d.width, d.height))
            .collect();
        format!("Lets paired devices see and control: {}", list.join(", "))
    }

    fn host_name(&self) -> String {
        self.options.name.clone()
    }

    fn clients(&self) -> Vec<oriel_client::launcher::SharedClient> {
        self.control
            .status()
            .clients
            .into_iter()
            .map(|c| oriel_client::launcher::SharedClient {
                id: c.id,
                name: c.name,
                address: c.address.ip().to_string(),
                codec: c.codec,
            })
            .collect()
    }

    fn kick(&self, id: usize) {
        self.control.kick(id);
    }

    fn allow_pairing(&mut self) -> Option<String> {
        Some(self.control.allow_pairing())
    }

    fn pairing_pin(&self) -> Option<String> {
        self.control.pairing_pin()
    }
}

/// GUI-subsystem builds have no console; reuse the one of the terminal we
/// were started from, if any, so command output shows up there.
#[cfg(windows)]
fn attach_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
