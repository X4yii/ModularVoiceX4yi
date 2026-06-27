# Arquitectura de MVX

## Procesos

```text
┌─────────────────────────────────────────────────────────────────────┐
│ mvxd (daemon)                                                       │
│                                                                     │
│  IPC Unix socket ──► AudioCommand channel                           │
│       ▲                   │                                         │
│       │                   ▼                                         │
│  mvx / mvx-tray     PipeWire main loop                              │
│                     ├── mvx.capture stream                          │
│                     ├── mvx.output stream                           │
│                     ├── link-factory FL/FR                          │
│                     └── mvx_mic Audio/Source/Virtual                │
│                                                                     │
│  worker de hablante (fuera del RT)                                  │
│    ring SPSC ← muestras mono del callback                           │
│    resample 48k→16k · VAD · CAM++ · scoring                         │
└─────────────────────────────────────────────────────────────────────┘
```

El daemon es dueño de todos los objetos PipeWire y del estado de presets. La TUI y el tray son clientes ligeros: pueden reiniciarse sin interrumpir el audio.

## Cadena de audio

1. `mvx.capture` (Input, F32LE, 48 kHz, 2 ch) se conecta a la fuente física mediante `target.object` + `AUTOCONNECT`.
2. El callback de captura:
   - lee el `chunk` del buffer;
   - downmixea a mono;
   - envía la señal cruda al ring del worker de hablante (si `Solo mi voz` está activo);
   - aplica la cadena DSP:
     - compuerta de hablante (con lookahead configurable);
     - RNNoise opcional;
     - pitch/formantes con Signalsmith Stretch;
     - ganancia del preset;
     - ganancia maestra + mute;
     - limitador de seguridad;
   - escribe dos muestras (L/R) en un `rtrb::RingBuffer<f32>`.
3. El callback de reproducción:
   - limita el bloque a `quantum` frames (256 o 128);
   - mantiene un pre-roll de 2 bloques (descarta exceso al arrancar);
   - lee del ring y escribe F32LE intercalado;
   - cuenta underruns/overflows.
4. `mvx.output` se enlaza manualmente a `mvx_mic`:
   - `link.output.node/port` = nodo y puerto del stream de playback;
   - `link.input.node/port` = nodo y puerto `input_FL`/`input_FR` del micrófono virtual.
5. `mvx_mic` es un nodo nativo:

```text
factory.name   = support.null-audio-sink
media.class    = Audio/Source/Virtual
node.name      = mvx_mic
audio.position = FL,FR
```

No se usa `pw-loopback`, `pactl` ni un sink físico intermedio.

## Presets y filtros

`~/.config/mvx/config.toml` guarda audio, DSP, UI y el preset activo.
`~/.config/mvx/presets.toml` guarda las definiciones:

```toml
version = 1

[[presets]]
id = "voz_grave"
name = "Voz grave"
builtin = true
pitch_semitones = -5.0
formant_semitones = -2.0
quality = "ultra"

[[presets.filters]]
type = "noise_reduction"
enabled = false
strength = 0.75

[[presets.filters]]
type = "gain"
enabled = true
gain_db = 0.0

[[presets.filters]]
type = "speaker_lock"
enabled = false
profile = ""
threshold = 0.72
hold_ms = 300
lookahead_ms = 0
```

Los built-in se pueden editar y restaurar. Los personalizados se pueden borrar. Pitch, formantes, intensidad de NR, umbral y hold se actualizan en vivo sin reconstruir. Los cambios que afectan la estructura DSP (pasar de bypass a Stretch o viceversa, calidad, NR on/off, Solo mi voz, perfil o lookahead) reconstruyen el camino de audio; ganancia y mute usan atómicos.

### Calidad DSP

| Perfil | Bloque | Latencia aproximada |
|---|---:|---:|
| `Ultra` | 512 | 10.7 ms |
| `Equilibrado` | 1024 | 21.3 ms |
| `Alta` | 2048 | 42.7 ms |

Si el preset no tiene pitch ni formantes, Signalsmith se omite y no agrega latencia.

## Tiempo real

- Los callbacks de captura y reproducción usan `PW_STREAM_FLAG_RT_PROCESS`.
- En esos callbacks **no** hay allocations, locks, logging ni syscalls.
- La comunicación con la UI es por atómicos (`AtomicU32`/`AtomicU64`) y rings SPSC.
- Los comandos de control llegan por `pw::channel` y se ejecutan en el hilo del main loop, nunca en el callback RT.
- RNNoise procesa frames de 480 muestras y agrega un frame de latencia (10 ms) cuando está activo.
- El worker de hablante no toca el callback RT: consume un ring, resamplea, corre VAD/ONNX y publica un booleano en un `AtomicBool`.
- Las perillas de pitch/formantes e intensidad de NR usan atómicos compartidos; el callback solo llama a los setters de Signalsmith o lee la intensidad cuando el valor cambió.
- Umbral y hold de la compuerta se leen desde `SharedState` en cada análisis, fuera del RT.

## Identidad de voz

El worker de hablante:

1. acumula bloques de 100 ms desde el ring;
2. resamplea linealmente de 48 kHz a 16 kHz;
3. alimenta Silero VAD;
4. mantiene una ventana de 0,5 s;
5. cada `speaker_analysis_hz` (4 por defecto) calcula un embedding CAM++ y su similitud coseno contra el perfil activo;
6. publica `score`, `vad`, `target` y el estado de disponibilidad.

La compuerta usa `threshold`, `hold_ms` y `lookahead_ms`:

- `threshold`: similitud mínima (0.72 por defecto).
- `hold_ms`: tiempo que la compuerta sigue abierta después de la última coincidencia (300 ms por defecto).
- `lookahead_ms`: retardo del audio para alinear la decisión con muestras anteriores (0 por defecto para no sumar latencia).

Si el modelo o el perfil no están disponibles, la compuerta queda desactivada y el audio pasa sin filtrar. El enrolamiento también funciona con la compuerta en ese estado, porque el worker sigue corriendo para poder grabar el perfil.

## Formato

- Frecuencia interna: 48 kHz.
- Muestras: `f32` intercalado (`F32LE`).
- Canales internos del DSP: mono; salida duplicada a estéreo.
- Quantum: configurable (`256` por defecto, `128` perfil ultra).

## IPC

Socket: `$XDG_RUNTIME_DIR/mvx/mvxd.sock` (permisos `0600`).

Protocolo: una línea JSON por request/response. Versión actual: **2**.

```json
{"cmd":"hello","protocol":2}
{"cmd":"get_state"}
{"cmd":"set_input","name":"alsa_input..."}
{"cmd":"set_gain","gain_db":-3.0}
{"cmd":"set_mute","mute":true}
{"cmd":"apply_preset","id":"voz_grave"}
{"cmd":"upsert_preset","preset":{...},"activate":true}
{"cmd":"delete_preset","id":"custom_1"}
{"cmd":"reset_preset","id":"voz_grave"}
{"cmd":"enroll_voice","name":"mi_voz","seconds":10}
{"cmd":"delete_voice","name":"mi_voz"}
{"cmd":"install_models"}
{"cmd":"shutdown"}
```

`get_state` devuelve un `Snapshot` con dispositivos, entrada seleccionada, formato, quantum, peaks, latencia, presets, estado de la compuerta, perfiles, modelos y contadores.

## Enlace manual

El stream de playback **no** usa autoconnect hacia `mvx_mic`: PipeWire/WirePlumber lo conectaría al sink por defecto. En su lugar:

1. el registry mantiene un cache de nodos y puertos;
2. cuando aparecen los puertos `output_FL/FR` y `input_FL/FR`, el daemon crea dos objetos `link-factory`;
3. si el link falla, se reintenta en el siguiente tick.

Esto permite control explícito de la topología y evita depender de la política del session manager.

## Próximos pasos

- Separación real de voces solapadas (no solo gating).
- Supresión de palabras/nombres concretos con KWS/audio tagging.
- Resampler adaptativo para deriva entre el reloj de la entrada física y el driver virtual.
- Medición real de latencia E2E.
- Clonación neuronal de timbre.
