# MVX · ModularVoiceX4yi

Modulador de voz personal, nativo para Linux/PipeWire, con salida a un **micrófono virtual**, presets editables y una TUI pensada para usarse con ratón.

El objetivo es tener control total de la voz: entrada física → cadena DSP → micrófono virtual. La clonación neuronal de voz queda como una etapa posterior.

## Estado actual

Funciona de punta a punta en PipeWire nativo:

- Captura desde la entrada física elegida y fuente virtual nativa `mvx_mic` (`Audio/Source/Virtual`).
- Ring buffer SPSC sin locks en el callback de audio.
- **Presets**: `Natural`, `Voz grave`, `Voz de ardilla` y presets personalizados.
- **Pitch y formantes independientes** con Signalsmith Stretch.
- **Reducción de ruido** opcional con RNNoise (`nnnoiseless`).
- **Solo mi voz**: verificación de hablante con CAM++ y VAD Silero; el audio de otras voces se atenúa.
- **Ganancia maestra + ganancia por preset**, mute y limitador de seguridad.
- **Calidad DSP** seleccionable: `Ultra` (~21.3 ms), `Equilibrado` (~42.7 ms) y `Alta` (~85.3 ms).
- Daemon `mvxd` con IPC Unix socket (protocolo v2).
- TUI en español con ratón completo (clic, arrastre, rueda), teclado físico y dos secciones: **General** y **Dispositivos**.
- Tray `mvx-tray` para hosts StatusNotifierItem (Noctalia, Waybar, KDE, etc.).
- Servicios systemd de usuario y script de instalación.

Todavía **no** hay separación real de voces solapadas, supresión de palabras/nombres ni clonación neuronal. `Solo mi voz` es una compuerta por similitud de hablante, no un separador de fuentes.

## Requisitos

- Linux con PipeWire 1.2+ y WirePlumber (o un gestor de sesión equivalente).
- Rust estable reciente (probado con 1.99).
- Headers/libs de desarrollo de PipeWire (`pipewire`, `libspa`).
- `clang`/`libclang` para compilar las dependencias de ONNX.
- `curl` para descargar los modelos de voz.
- Opcional: `pavucontrol`, `xdg-terminal-exec` o un terminal compatible.

En Arch/CachyOS:

```bash
sudo pacman -S --needed pipewire pipewire-audio wireplumber rust base-devel clang curl
```

La primera compilación descarga las librerías estáticas de `sherpa-onnx`; por eso puede tardar más que una compilación normal.

## Compilar

```bash
cargo build --release
```

Binarios:

- `target/release/mvxd` — daemon de audio.
- `target/release/mvx` — cliente/TUI.
- `target/release/mvx-tray` — icono de bandeja.

## Ejecutar

Después de instalar:

```bash
mxv
```

Ese único comando arranca el daemon si hace falta, inicia el tray (una sola instancia) y abre la TUI. Al cerrar la TUI con `q`, el daemon y el tray siguen vivos; la próxima `mxv` se reconecta. `mvx` hace exactamente lo mismo.

Para desarrollo sin instalar:

```bash
./target/release/mvxd
./target/release/mvx tui
```

`mvx tui` arranca `mvxd` y `mvx-tray` automáticamente si no están corriendo y los encuentra en `PATH` o junto al binario.

En aplicaciones de voz (Discord, navegador, OBS, etc.) elegí **MVX Microphone** como micrófono. También podés verlo en `pavucontrol` → *Grabación* / *Dispositivos de entrada*.

## Modelos de voz

`Solo mi voz` necesita dos modelos ONNX (~30 MB en total):

```bash
mxv models install   # descarga CAM++ y Silero VAD
mxv models status    # muestra rutas y perfiles
```

Se guardan en:

```text
~/.local/share/mvx/models/3dspeaker_campplus_zh_en.onnx
~/.local/share/mvx/models/silero_vad.onnx
~/.local/share/mvx/voices/<perfil>.json
```

El modelo CAM++ es multilingüe (chino/inglés) y se usa como extractor de embeddings; no requiere que hables en esos idiomas para verificar tu timbre.

### Enrolar tu voz

1. Abrí la TUI (`mxv`).
2. En **General**, activá **Solo mi voz** o pulsá **Enrolar voz**.
3. Escribí un nombre de perfil y confirmá.
4. Hablá con naturalidad durante 10 segundos.
5. Ajustá **Umbral de coincidencia** si tu score habitual queda cerca del límite.

El perfil queda asociado al preset activo. Después de enrolar, el worker recarga el perfil y empieza a comparar la voz entrante.

## Uso de la TUI

La TUI se maneja con ratón de forma tradicional:

- **Clic** en tabs, presets, botones y perfiles.
- **Arrastrar** sobre un slider para cambiar el valor.
- **Rueda** sobre un slider para ajustarlo; sobre la lista de presets, cambia la selección.
- Los botones deshabilitados se muestran en gris.

También hay teclado físico para todo:

| Tecla | Acción |
|---|---|
| `Tab` / `g` / `d` | Cambiar entre General y Dispositivos |
| `j` / `k` o flechas | Mover la selección |
| `Enter` | Aplicar preset o entrada |
| `n` | Nuevo preset personalizado |
| `r` | Restaurar el preset built-in seleccionado |
| `m` | Alternar mute |
| `s` | Alternar Solo mi voz |
| `e` | Enrolar un perfil de voz |
| `i` | Instalar modelos de voz |
| `+` / `-` | Ajustar volumen maestro |
| `[` / `]` | Ajustar pitch |
| `q` / `Esc` | Cerrar la TUI (el daemon sigue vivo) |

En el modal de texto se escribe con el teclado físico; no hay teclado en pantalla.

### Presets

- Los tres built-in se pueden editar y después recuperar con **Restaurar**.
- **Nuevo preset** crea una copia personalizable del preset actual.
- `Voz de ardilla` sube pitch y formantes juntos (+6 semitonos), que es lo que produce el timbre tipo Alvin y las ardillas.
- Los cambios de pitch, formantes, intensidad de NR, ganancia y Solo mi voz se aplican en vivo; pitch, formantes, NR y ganancia se suavizan para evitar zipper noise. Cambiar entre presets con la misma estructura DSP ya no reconstruye el camino de audio.
- La ganancia maestra se guarda en `config.toml`; la ganancia del preset se suma a la maestra.

## CLI

```bash
mxv status          # snapshot JSON del daemon
mxv devices         # entradas físicas detectadas
mxv doctor          # diagnóstico de socket, formato, modelos, xruns, etc.
mxv models install  # descargar modelos de voz
mxv models status   # ver modelos y perfiles
mxv shutdown        # apagar el daemon

# `mvx` es equivalente; ambos son el mismo binario.
```

## Configuración

Archivo: `~/.config/mvx/config.toml`

```toml
config_version = 2

[audio]
input = "alsa_input.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__Mic1__source"
sample_rate = 48000
quantum = 256
gain_db = 0.0
mute = false
virtual_source_name = "mvx_mic"
virtual_source_description = "MVX Microphone"

[dsp]
default_quality = "balanced"
# speaker_model = "/ruta/al/modelo_campplus.onnx"
# vad_model = "/ruta/al/silero_vad.onnx"
speaker_analysis_hz = 4

[presets]
active = "natural"

[ui]
mouse = true
last_tab = "general"
```

Los presets viven en `~/.config/mvx/presets.toml`, separados de la configuración de audio/UI. Cada preset guarda pitch, formantes, calidad y filtros opcionales (NR, ganancia, Solo mi voz). El daemon crea el archivo con los built-in si no existe.

`quantum` acepta el perfil normal (`256`) o ultra (`128`). El daemon fuerza ese quantum en su nodo virtual mientras corre; cambiarlo afecta al grafo de PipeWire de la sesión.

## Arquitectura

```text
mic física
   │  stream Input (F32LE, 48 kHz, 2 ch)
   ▼
mvx.capture ── callback RT ──┬── ring SPSC ── callback RT ── mvx.output
                             │                                  │ link-factory
                             │                                  ▼
                             │                            mvx_mic (Audio/Source/Virtual)
                             │                                  │
                             │                                  ▼
                             │                           apps / navegador / Discord
                             │
                             └── ring SPSC ── worker de hablante
                                                ├── resample 48k → 16k
                                                ├── Silero VAD
                                                ├── CAM++ embedding
                                                └── score + decisión de compuerta
```

Cadena DSP en el callback de captura:

```text
mono → [Solo mi voz] → [RNNoise] → [pitch/formantes] → ganancia preset
     → ganancia maestra/mute → limitador → ring de salida
```

- `mvxd` es el único dueño de PipeWire y del DSP.
- `mvx` y `mvx-tray` son clientes IPC; pueden cerrarse y volver a abrirse sin cortar el audio.
- El callback de proceso no asigna memoria, no bloquea y no usa locks: solo ring buffer y atómicos.
- El worker de hablante corre fuera del hilo RT, con un hilo ONNX, prioridad baja y sin GPU por defecto.
- El formato interno es 48 kHz, `f32`, estéreo. La captura se downmixea a mono para el DSP y se duplica a L/R en la salida.

Más detalle en [`docs/architecture.md`](docs/architecture.md) y [`docs/latency.md`](docs/latency.md).

## Limitaciones conocidas

- **Solo mi voz** es una compuerta, no una separación de fuentes. Si dos personas hablan a la vez, la voz ajena puede filtrarse mientras el score siga alto.
- El análisis de hablante usa una ventana de ~0,5 s; los primeros instantes de una voz nueva pueden pasar antes de que la compuerta reaccione.
- El perfil es sensible al micrófono y a la posición. Si cambiás de micrófono, volvé a enrolar.
- La captura y la reproducción son dos streams de PipeWire; el puente usa un ring con pre-roll acotado. Falta un resampler adaptativo para absorber deriva de reloj entre dispositivos.
- El tray requiere un host SNI.
- La latencia mostrada es una estimación (quantum + DSP + buffer del ring), no una medición E2E.
- La supresión de palabras/nombres concretos y la clonación neuronal todavía no están implementadas.

## Roadmap

1. **M0** — plumbing PipeWire, TUI, tray, daemon. ✅
2. **M1** — cadena DSP: presets, pitch/formantes, NR, ganancia, limitador. ✅
3. **M2** — identidad de voz: VAD, embeddings, `Solo mi voz`, enrolamiento. ✅
4. **M3** — separación de hablantes solapados, supresión de palabras/nombres y clonación neuronal.
5. **M4** — grabación, análisis y ajuste automático opcional.

## Licencia

MIT.
