# MVX · ModularVoiceX4yi

Modulador de voz personal, nativo para Linux/PipeWire, con salida a un **micrófono virtual** y una TUI para controlar la cadena en tiempo real.

El objetivo del proyecto es tener control total de la voz: entrada física → DSP modular → micrófono virtual. La clonación neuronal de voz queda como una etapa posterior; primero se construye una base de audio estable y de baja latencia.

## Estado actual — M0

Funciona de punta a punta en PipeWire nativo:

- Captura desde la entrada física elegida.
- Fuente virtual nativa `mvx_mic` (`Audio/Source/Virtual`) creada con `adapter` + `support.null-audio-sink`.
- Enlace manual `mvx.output` → `mvx_mic` mediante `link-factory` (sin depender de `pactl`).
- Ring buffer SPSC sin locks en el callback RT.
- Passthrough con **gain** y **mute**.
- Daemon `mvxd` con IPC Unix socket.
- TUI `mvx` en español: dispositivos, medidores, estado, selección de entrada, mute y gain.
- Tray `mvx-tray` para hosts StatusNotifierItem (Noctalia, Waybar, KDE, etc.).
- Servicios systemd de usuario y script de instalación.

Todavía **no** están implementados RNNoise, pitch/formant, EQ, compresor, presets ni clonación neuronal. La arquitectura está preparada para agregarlos.

## Requisitos

- Linux con PipeWire 1.2+ y WirePlumber (o un gestor de sesión equivalente).
- Rust estable reciente (probado con 1.99).
- `pipewire` headers/libs de desarrollo.
- Opcional: `pavucontrol`, `xdg-terminal-exec` o un terminal compatible.

En Arch/CachyOS:

```bash
sudo pacman -S --needed pipewire pipewire-audio wireplumber rust base-devel
```

## Compilar

```bash
cargo build --release
```

Binarios:

- `target/release/mvxd` — daemon de audio.
- `target/release/mvx` — cliente/TUI.
- `target/release/mvx-tray` — icono de bandeja.
- `mxv` — alias instalado de `mvx` para el lanzamiento de un solo comando.

## Ejecutar

Después de instalar:

```bash
mxv
```

Ese único comando arranca el daemon si hace falta, inicia el tray (una sola
instancia) y abre la TUI. Al cerrar la TUI con `q`, el daemon y el tray siguen
vivos; la próxima `mxv` se reconecta. `mvx` hace exactamente lo mismo.

Para desarrollo sin instalar:

```bash
./target/release/mvxd
./target/release/mvx tui
```

`mvx tui` arranca `mvxd` y `mvx-tray` automáticamente si no están corriendo y
los encuentra en `PATH` o junto al binario.

En aplicaciones de voz (Discord, navegador, OBS, etc.) elegí **MVX Microphone** como micrófono. También podés verlo en `pavucontrol` → *Grabación* / *Dispositivos de entrada*.

## Uso de la TUI

| Tecla | Acción |
|---|---|
| `j` / `k` o flechas | Moverse por las entradas |
| `Enter` | Aplicar la entrada seleccionada |
| `m` | Alternar mute |
| `+` / `-` | Ajustar gain en pasos de 1 dB |
| `r` | Refrescar estado |
| `q` / `Esc` | Cerrar la TUI (el daemon sigue vivo) |

## CLI

```bash
mxv status      # snapshot JSON del daemon
mxv devices     # entradas físicas detectadas
mxv doctor      # diagnóstico de socket, formato, xruns, etc.
mxv shutdown    # apagar el daemon

# `mvx` es equivalente; ambos son el mismo binario.
```

## Tray

```bash
mvx-tray
```

El menú permite abrir la TUI, alternar mute, abrir `pavucontrol`, apagar el daemon o cerrar el tray. El icono usa el nombre de tema `audio-input-microphone` (y su variante muted).

## Instalación de usuario

```bash
./scripts/install.sh
mxv
```

El script instala en `~/.local/bin` (incluido el alias `mxv`), los servicios en
`~/.config/systemd/user` y el lanzador en `~/.local/share/applications`.

Opcional, para que daemon y tray sobrevivan reinicios de sesión:

```bash
systemctl --user daemon-reload
systemctl --user enable --now mvxd.service
systemctl --user enable --now mvx-tray.service
```

## Configuración

Archivo: `~/.config/mvx/config.toml`

```toml
[audio]
input = "alsa_input.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__Mic1__source"
sample_rate = 48000
quantum = 256
gain_db = 0.0
mute = false
virtual_source_name = "mvx_mic"
virtual_source_description = "MVX Microphone"
```

`quantum` acepta el perfil normal (`256`) o ultra (`128`). El daemon fuerza ese quantum en su nodo virtual mientras corre; cambiarlo afecta al grafo de PipeWire de la sesión.

La selección de entrada, gain y mute se guardan automáticamente desde la TUI.

## Arquitectura

```text
mic física
   │  stream Input (F32LE, 48 kHz, 2 ch)
   ▼
mvx.capture ── callback RT ── ring SPSC ── callback RT ── mvx.output
                                                              │ link-factory
                                                              ▼
                                                        mvx_mic (Audio/Source/Virtual)
                                                              │
                                                              ▼
                                                     apps / navegador / Discord
```

- `mvxd` es el único dueño de PipeWire y del DSP.
- `mvx` y `mvx-tray` son clientes IPC; pueden cerrarse y volver a abrirse sin cortar el audio.
- El callback de proceso no asigna memoria, no bloquea y no usa locks: solo ring buffer y atómicos.
- El formato interno es 48 kHz, `f32`, estéreo. La captura se downmixea a mono para el DSP y se duplica a L/R en la salida.

Más detalle en [`docs/architecture.md`](docs/architecture.md) y [`docs/latency.md`](docs/latency.md).

## Limitaciones conocidas

- La captura y la reproducción son dos streams de PipeWire; el puente usa un ring con pre-roll acotado. En el futuro se agregará un resampler adaptativo para absorber deriva de reloj entre dispositivos.
- No hay todavía cadena DSP modular (solo gain/mute).
- El tray requiere un host SNI (Noctalia lo soporta).
- La latencia mostrada es una estimación (buffer del ring + quantum), no una medición E2E.

## Roadmap

1. **M0** — plumbing PipeWire, TUI, tray, daemon. ✅
2. **M1** — cadena DSP modular: HPF, gate, EQ, compresor, limiter, presets, A/B.
3. **M2** — RNNoise / supresión de ruido y pitch/formant.
4. **M3** — clonación neuronal de voz y modelos de timbre.
5. **M4** — grabación, análisis y ajuste automático opcional.

## Licencia

MIT.
