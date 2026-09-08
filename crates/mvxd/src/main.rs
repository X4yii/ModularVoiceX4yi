use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
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

    mvx_core::ensure_runtime_dir().context("creando el directorio runtime")?;

    // Exclusive instance lock: only one mvxd daemon can run per user.
    let _instance_lock = match mvx_core::acquire_instance_lock("mvxd.lock")? {
        Some(lock) => lock,
        None => {
            info!("mvxd ya está en ejecución (bloqueo activo en mvxd.lock)");
            return Ok(());
        }
    };

    let (config, parse_error) = Config::load_with_status();
    if !mvx_core::config_path().exists() && !parse_error {
        if let Err(err) = config.save() {
            warn!("no se pudo guardar la configuración inicial: {err}");
        }
    }

    let socket_path = mvx_core::socket_path();
    let socket_guard = if args.no_ipc {
        None
    } else {
        Some(SocketGuard::bind(socket_path.clone())?)
    };

    let shared = Arc::new(SharedState::new(&config));
    if parse_error {
        shared.set_config_parse_error(true);
        shared.set_error(Some(
            "config.toml corrupto: se usó configuración por defecto (respaldo en config.toml.bak)"
                .into(),
        ));
    }
    let engine = AudioEngine::new(shared.clone(), config)
        .context("no se pudo inicializar el motor de audio")?;
    let sender = engine.sender();

    let shutdown = Arc::new(AtomicBool::new(false));
    let installing_models = Arc::new(AtomicBool::new(false));

    if let Some(_guard) = &socket_guard {
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("no se pudo escuchar en {}", socket_path.display()))?;
        #[cfg(unix)]
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

        let sender_ipc = sender.clone();
        let shared_ipc = shared.clone();
        let shutdown_ipc = shutdown.clone();
        let installing_ipc = installing_models.clone();
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
                            let installing = installing_ipc.clone();
                            let _ =
                                thread::Builder::new()
                                    .name("mvx-client".into())
                                    .spawn(move || {
                                        if let Err(err) = handle_client(
                                            stream, sender, shared, shutdown, installing,
                                        ) {
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
    installing_models: Arc<AtomicBool>,
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
            Request::ApplyPreset { id } => match sender.send(AudioCommand::ApplyPreset { id }) {
                Ok(()) => Response::Ok,
                Err(_) => Response::error("el motor de audio no está disponible"),
            },
            Request::UpsertPreset { preset, activate } => {
                match sender.send(AudioCommand::UpsertPreset { preset, activate }) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::error("el motor de audio no está disponible"),
                }
            }
            Request::DeletePreset { id } => match sender.send(AudioCommand::DeletePreset { id }) {
                Ok(()) => Response::Ok,
                Err(_) => Response::error("el motor de audio no está disponible"),
            },
            Request::ResetPreset { id } => match sender.send(AudioCommand::ResetPreset { id }) {
                Ok(()) => Response::Ok,
                Err(_) => Response::error("el motor de audio no está disponible"),
            },
            Request::EnrollVoice { name, seconds } => {
                match sender.send(AudioCommand::EnrollVoice { name, seconds }) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::error("el motor de audio no está disponible"),
                }
            }
            Request::DeleteVoice { name } => {
                match sender.send(AudioCommand::DeleteVoice { name }) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::error("el motor de audio no está disponible"),
                }
            }
            Request::SetBypass { bypass } => {
                match sender.send(AudioCommand::SetBypass(bypass)) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::error("el motor de audio no está disponible"),
                }
            }
            Request::SetMonitor { enabled, gain_db } => {
                match sender.send(AudioCommand::SetMonitor { enabled, gain_db }) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::error("el motor de audio no está disponible"),
                }
            }
            Request::StartLoopRecord => {
                match sender.send(AudioCommand::StartLoopRecord) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::error("el motor de audio no está disponible"),
                }
            }
            Request::StopLoopTest => {
                match sender.send(AudioCommand::StopLoopTest) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::error("el motor de audio no está disponible"),
                }
            }
            Request::TriggerSound { id } => {
                match sender.send(AudioCommand::TriggerSound { id }) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::error("el motor de audio no está disponible"),
                }
            }
            Request::StopSounds => {
                match sender.send(AudioCommand::StopSounds) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::error("el motor de audio no está disponible"),
                }
            }
            Request::InstallModels => {
                if installing_models.swap(true, Ordering::Relaxed) {
                    Response::error("la descarga de modelos ya está en curso")
                } else {
                    spawn_model_install(shared.clone(), installing_models.clone(), sender.clone());
                    Response::Ok
                }
            }
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

fn spawn_model_install(shared: Arc<SharedState>, busy: Arc<AtomicBool>, sender: CommandSender) {
    thread::Builder::new()
        .name("mvx-model-download".into())
        .spawn(move || {
            shared.set_speaker_status("descargando modelos de voz…");
            let result = install_models();
            busy.store(false, Ordering::Relaxed);
            match result {
                Ok(()) => {
                    shared.set_models_installed(true);
                    shared.set_speaker_status("modelos de voz instalados");
                    // If Solo mi voz was enabled while the models were absent,
                    // the speaker worker could not start. Rebuild now that the
                    // files are available.
                    let _ = sender.send(AudioCommand::ReloadAudio);
                }
                Err(err) => {
                    shared.set_models_installed(false);
                    shared.set_speaker_status(format!("error al descargar modelos: {err}"));
                    shared.set_error(Some(format!("modelos de voz: {err}")));
                }
            }
        })
        .ok();
}

fn install_models() -> Result<()> {
    let dir = mvx_core::models_dir();
    std::fs::create_dir_all(&dir)?;
    let downloads = [
        (
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
            mvx_core::speaker_model_path(),
        ),
        (
            "https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/data/silero_vad.onnx",
            mvx_core::vad_model_path(),
        ),
    ];
    for (url, dest) in downloads {
        if dest.exists() {
            continue;
        }
        let part = dest.with_extension("onnx.part");
        info!("descargando {} -> {}", url, dest.display());
        download(url, &part)?;
        std::fs::rename(&part, &dest)?;
    }
    Ok(())
}

fn download(url: &str, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("curl")
        .args([
            "-L",
            "--fail",
            "--silent",
            "--show-error",
            "--retry",
            "2",
            "--connect-timeout",
            "20",
            "-o",
        ])
        .arg(dest)
        .arg(url)
        .status()
        .context("no se pudo ejecutar curl (¿está instalado?)")?;
    if !status.success() {
        anyhow::bail!("curl terminó con {status}");
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
