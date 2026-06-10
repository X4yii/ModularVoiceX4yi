# Roadmap

## M0 — Base PipeWire (actual)

- [x] Fuente virtual nativa y enlace manual.
- [x] Daemon IPC y reconexión de clientes.
- [x] TUI y tray.
- [x] Gain/mute persistidos.
- [x] systemd de usuario.

## M1 — DSP modular

- [ ] Trait `DspNode` con `prepare`, `process`, `reset`.
- [ ] Cadena ordered con parámetros suavizados.
- [ ] High-pass 20–300 Hz.
- [ ] Noise gate con threshold/ratio/attack/release.
- [ ] EQ paramétrico de 3–5 bandas.
- [ ] Compresor y limiter.
- [ ] Presets TOML y A/B.
- [ ] Medición de reducción de ganancia.

## M2 — Voz

- [ ] RNNoise (nnnoiseless) con VAD.
- [ ] Pitch/formant sin destruir formantes.
- [ ] De-esser.
- [ ] Monitor de voz opcional (solo para auriculares).

## M3 — Clonación neuronal

- [ ] Pipeline de análisis de muestras objetivo.
- [ ] Embeddings de timbre.
- [ ] Conversión voz-a-voz con modelo local.
- [ ] Control de similitud vs. inteligibilidad.
