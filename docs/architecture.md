# Arquitectura de MVX

## Procesos

```text
┌─────────────────────────────────────────────────────────────┐
│ mvxd (daemon)                                               │
│                                                             │
│  IPC Unix socket  ──►  AudioCommand channel                 │
│       ▲                    │                                │
│       │                    ▼                                │
│  mvx / mvx-tray      PipeWire main loop                     │
│                      ├── mvx.capture stream                 │
│                      ├── mvx.output stream                  │
│                      ├── link-factory FL/FR                 │
│                      └── mvx_mic Audio/Source/Virtual       │
└─────────────────────────────────────────────────────────────┘
```

El daemon es dueño de todos los objetos PipeWire. La TUI y el tray son clientes ligeros: pueden reiniciarse sin interrumpir el audio.

## Cadena de audio M0

1. `mvx.capture` (Input, F32LE, 48 kHz, 2 ch) se conecta a la fuente física mediante `target.object` + `AUTOCONNECT`.
2. El callback de captura:
   - lee el `chunk` del buffer;
   - downmixea a mono;
   - aplica `gain_db` y `mute`;
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

## Tiempo real

- Los callbacks de captura y reproducción usan `PW_STREAM_FLAG_RT_PROCESS`.
- En esos callbacks **no** hay allocations, locks, logging ni syscalls.
- La comunicación con la UI es por atómicos (`AtomicU32`/`AtomicU64`) y el ring SPSC.
- Los comandos de control llegan por `pw::channel` y se ejecutan en el hilo del main loop, nunca en el callback RT.

## Formato

- Frecuencia interna: 48 kHz.
- Muestras: `f32` intercalado (`F32LE`).
- Canales internos del DSP: mono; salida duplicada a estéreo.
- Quantum: configurable (`256` por defecto, `128` perfil ultra).

## IPC

Socket: `$XDG_RUNTIME_DIR/mvx/mvxd.sock` (permisos `0600`).

Protocolo: una línea JSON por request/response.

```json
{"cmd":"hello","protocol":1}
{"cmd":"get_state"}
{"cmd":"set_input","name":"alsa_input..."}
{"cmd":"set_gain","gain_db":-3.0}
{"cmd":"set_mute","mute":true}
{"cmd":"shutdown"}
```

`get_state` devuelve un `Snapshot` con dispositivos, entrada seleccionada, formato, quantum, peaks, contadores y error actual.

## Enlace manual

El stream de playback **no** usa autoconnect hacia `mvx_mic`: PipeWire/WirePlumber lo conectaría al sink por defecto. En su lugar:

1. el registry mantiene un cache de nodos y puertos;
2. cuando aparecen los puertos `output_FL/FR` y `input_FL/FR`, el daemon crea dos objetos `link-factory`;
3. si el link falla, se reintenta en el siguiente tick.

Esto permite control explícito de la topología y evita depender de la política del session manager.

## Próximos pasos

- Cadena DSP por nodos (`Node` trait) con parámetros atómicos y presets.
- Resampler adaptativo para deriva entre el reloj de la entrada física y el driver virtual.
- Medición real de latencia E2E.
