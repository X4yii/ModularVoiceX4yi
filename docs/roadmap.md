# Roadmap

## M0 — Base PipeWire ✅

- [x] Fuente virtual nativa y enlace manual.
- [x] Daemon IPC y reconexión de clientes.
- [x] TUI y tray.
- [x] Gain/mute persistidos.
- [x] systemd de usuario.

## M1 — DSP modular ✅

- [x] Presets TOML separados de `config.toml`.
- [x] Built-in `Natural`, `Voz grave`, `Voz de ardilla`, editables y restaurables.
- [x] Presets personalizados.
- [x] Pitch/formantes independientes con Signalsmith Stretch.
- [x] Calidad DSP (Ultra / Equilibrado / Alta).
- [x] RNNoise opcional con intensidad.
- [x] Ganancia maestra + ganancia por preset.
- [x] Limitador de seguridad.
- [x] TUI con ratón, sliders, modales y teclado físico.

## M2 — Identidad de voz ✅

- [x] Modelos CAM++ y Silero VAD descargables desde la TUI/CLI.
- [x] Worker de hablante fuera del hilo RT.
- [x] Resampling 48k→16k, VAD y embeddings.
- [x] Perfiles de voz persistentes.
- [x] Enrolamiento guiado.
- [x] `Solo mi voz` por preset con umbral, hold y lookahead.
- [x] Score/VAD/estado visibles en la TUI.

## M3 — Separación y contenido

- [ ] Separación real de voces solapadas (no solo compuerta).
- [ ] Supresión de palabras/nombres concretos con KWS/audio tagging.
- [ ] Detección de tipos de sonido (ladridos, llaves, etc.).
- [ ] Clonación neuronal de timbre.
- [ ] Conversión voz-a-voz local con control de similitud.

## M4 — Producto

- [ ] Resampler adaptativo para deriva de reloj.
- [ ] Medición real de latencia E2E.
- [ ] Grabación y análisis de muestras.
- [ ] Ajuste automático opcional.
- [ ] Empaquetado para más distribuciones.
