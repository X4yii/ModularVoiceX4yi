//! Line-delimited JSON protocol between `mvxd` and its clients.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use mvx_core::{Preset, Snapshot, PROTOCOL_VERSION, VERSION};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Hello { protocol: u32 },
    GetState,
    SetInput { name: String },
    SetGain { gain_db: f32 },
    SetMute { mute: bool },
    ApplyPreset { id: String },
    UpsertPreset { preset: Preset, activate: bool },
    DeletePreset { id: String },
    ResetPreset { id: String },
    EnrollVoice { name: String, seconds: u32 },
    DeleteVoice { name: String },
    SetBypass { bypass: bool },
    SetMonitor { enabled: bool, gain_db: f32 },
    StartLoopRecord,
    StopLoopTest,
    TriggerSound { id: String },
    StopSounds,
    ResetConfig { target: ConfigResetTarget },
    CloneVoiceValidate { audio_path: String },
    CloneVoiceSave { session: mvx_core::VoiceCloneSession },
    InstallModels,
    Ping,
    Shutdown,
}

pub use mvx_core::ConfigResetTarget;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Response {
    Hello { version: String, protocol: u32 },
    State { state: Snapshot },
    Ok,
    Pong,
    Error { message: String },
}

impl Response {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

pub fn hello_response() -> Response {
    Response::Hello {
        version: VERSION.to_string(),
        protocol: PROTOCOL_VERSION,
    }
}

pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            writer: stream,
            reader,
        })
    }

    pub fn request(&mut self, request: &Request) -> anyhow::Result<Response> {
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        self.writer.flush()?;

        let mut response = String::new();
        let n = self.reader.read_line(&mut response)?;
        if n == 0 {
            anyhow::bail!("el daemon cerró la conexión");
        }
        Ok(serde_json::from_str(response.trim_end())?)
    }

    pub fn hello(&mut self) -> anyhow::Result<Response> {
        let response = self.request(&Request::Hello {
            protocol: PROTOCOL_VERSION,
        })?;
        if let Response::Hello { protocol, .. } = &response {
            if *protocol != PROTOCOL_VERSION {
                anyhow::bail!(
                    "versión de protocolo incompatible: daemon={protocol}, cliente={PROTOCOL_VERSION}"
                );
            }
        }
        Ok(response)
    }

    pub fn state(&mut self) -> anyhow::Result<Snapshot> {
        match self.request(&Request::GetState)? {
            Response::State { state } => Ok(state),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn set_mute(&mut self, mute: bool) -> anyhow::Result<()> {
        match self.request(&Request::SetMute { mute })? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn set_bypass(&mut self, bypass: bool) -> anyhow::Result<()> {
        match self.request(&Request::SetBypass { bypass })? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn set_monitor(&mut self, enabled: bool, gain_db: f32) -> anyhow::Result<()> {
        match self.request(&Request::SetMonitor { enabled, gain_db })? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn apply_preset(&mut self, id: String) -> anyhow::Result<()> {
        match self.request(&Request::ApplyPreset { id })? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn trigger_sound(&mut self, id: String) -> anyhow::Result<()> {
        match self.request(&Request::TriggerSound { id })? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn stop_sounds(&mut self) -> anyhow::Result<()> {
        match self.request(&Request::StopSounds)? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn start_loop_record(&mut self) -> anyhow::Result<()> {
        match self.request(&Request::StartLoopRecord)? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn stop_loop_test(&mut self) -> anyhow::Result<()> {
        match self.request(&Request::StopLoopTest)? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn reset_config(&mut self, target: ConfigResetTarget) -> anyhow::Result<()> {
        match self.request(&Request::ResetConfig { target })? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn clone_voice_validate(&mut self, audio_path: String) -> anyhow::Result<()> {
        match self.request(&Request::CloneVoiceValidate { audio_path })? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }

    pub fn clone_voice_save(&mut self, session: mvx_core::VoiceCloneSession) -> anyhow::Result<()> {
        match self.request(&Request::CloneVoiceSave { session })? {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("respuesta inesperada: {other:?}"),
        }
    }
}

pub fn read_request(reader: &mut BufReader<UnixStream>) -> anyhow::Result<Option<Request>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(line.trim_end())?))
}

pub fn write_response(writer: &mut UnixStream, response: &Response) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}
