use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use mvx_audio::{AudioCommand, AudioEngine, CommandSender};
use mvx_core::{Config, SharedState};
use mvx_ipc::{hello_response, read_request, write_response, Request, Response};

#[derive(Debug, Parser)]
#[command(name = "mvxd", about = "MVX voice modulator daemon")]
struct Args {
    /// Print more diagnostics.
    #[arg(short, long)]
    verbose: bool,
    /// Do not create the Unix socket (only run the audio engine).
    #[arg(long)]
    no_ipc: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.verbose);

    let config = Config::load();
    if !mvx_core::config_path().exists() {
        if let Err(err) = config.save() {
            warn!("no se pudo guardar la configuración inicial: {err}");
        }
    }

    let socket_path = mvx_core::socket_path();
    if socket_path.exists() {
        if UnixStream::connect(&socket_path).is_ok() {
            info!("mvxd ya está en ejecución");
            return Ok(());
        }
        let _ = std::fs::remove_file(&socket_path);
    }

    mvx_core::ensure_runtime_dir().context("creando el directorio runtime")?;

    let shared = Arc::new(SharedState::new(&config));
    let engine = AudioEngine::new(shared.clone(), config)
        .context("no se pudo inicializar el motor de audio")?;
    let sender = engine.sender();

    let shutdown = Arc::new(AtomicBool::new(false));
    let socket_guard = if args.no_ipc {
        None
    } else {
        Some(SocketGuard::bind(socket_path.clone())?)
    };

    if let Some(_guard) = &socket_guard {
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("no se pudo escuchar en {}", socket_path.display()))?;
        #[cfg(unix)]
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

        let sender_ipc = sender.clone();
        let shared_ipc = shared.clone();
        let shutdown_ipc = shutdown.clone();
        thread::Builder::new()
            .name("mvx-ipc".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if shutdown_ipc.load(Ordering::Relaxed) {
                        break;
                    }
                    match stream {
                        Ok(stream) => {
                            let sender = sender_ipc.clone();
                            let shared = shared_ipc.clone();
                            let shutdown = shutdown_ipc.clone();
                            let _ =
                                thread::Builder::new()
                                    .name("mvx-client".into())
                                    .spawn(move || {
                                        if let Err(err) =
                                            handle_client(stream, sender, shared, shutdown)
                                        {
                                            warn!("cliente IPC: {err:#}");
                                        }
                                    });
                        }
                        Err(err) => warn!("accept IPC: {err}"),
                    }
                }
            })
            .context("no se pudo crear el hilo IPC")?;
    }

    let signal_sender = sender.clone();
    ctrlc::set_handler(move || {
        let _ = signal_sender.send(AudioCommand::Shutdown);
    })
    .context("no se pudo instalar el manejador de señales")?;

    info!(
        "mvxd iniciado; fuente virtual: {}",
        shared.snapshot().virtual_source
    );
    let result = engine.run();
    shutdown.store(true, Ordering::Relaxed);
    if let Some(guard) = socket_guard {
        drop(guard);
    }
    if let Err(err) = &result {
        error!("motor de audio: {err:#}");
    }
    result
}

fn handle_client(
    stream: UnixStream,
    sender: CommandSender,
    shared: Arc<SharedState>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let Some(request) = read_request(&mut reader)? else {
            break;
        };
        let response = match request {
            Request::Hello { protocol } => {
                if protocol != mvx_core::PROTOCOL_VERSION {
                    Response::error(format!(
                        "protocolo incompatible: cliente={protocol}, daemon={}",
                        mvx_core::PROTOCOL_VERSION
                    ))
                } else {
                    hello_response()
                }
            }
            Request::GetState => Response::State {
                state: shared.snapshot(),
            },
            Request::SetInput { name } => match sender.send(AudioCommand::SetInput(name)) {
                Ok(()) => Response::Ok,
                Err(_) => Response::error("el motor de audio no está disponible"),
            },
            Request::SetGain { gain_db } => match sender.send(AudioCommand::SetGain(gain_db)) {
                Ok(()) => Response::Ok,
                Err(_) => Response::error("el motor de audio no está disponible"),
            },
            Request::SetMute { mute } => match sender.send(AudioCommand::SetMute(mute)) {
                Ok(()) => Response::Ok,
                Err(_) => Response::error("el motor de audio no está disponible"),
            },
            Request::Ping => Response::Pong,
            Request::Shutdown => {
                let _ = sender.send(AudioCommand::Shutdown);
                shutdown.store(true, Ordering::Relaxed);
                Response::Ok
            }
        };
        write_response(&mut writer, &response)?;
    }
    Ok(())
}

fn init_logging(verbose: bool) {
    let default = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("mvxd={default},mvx_audio={default}")));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

struct SocketGuard(PathBuf);

impl SocketGuard {
    fn bind(path: PathBuf) -> Result<Self> {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        Ok(Self(path))
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
