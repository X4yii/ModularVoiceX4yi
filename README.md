# MVX · ModularVoiceX4yi (MVX 1.0 Pro)

> **Modulador y Procesador de Voz Profesional de Estudio para GNU/Linux, nativo sobre PipeWire.**

MVX transforma tu micrófono físico en un **micrófono virtual de calidad de estudio** (`mvx_mic`) con latencia ultra-baja (<15ms), 100% libre de bloqueos en tiempo real, con Voice Lab completo, Sidetone (monitoreo en auriculares), Voice Tester Loop, Soundboard con ducking automático y verificación biométrica de voz ("Solo mi voz").

---

## Características Principales

### 🎙️ Voice Lab Profesional (Rack DSP en Tiempo Real)
- **Pitch & Formant Shifter:** Modulación independiente de tono y tamaño de garganta con Signalsmith Stretch.
- **Filtro Paso Alto (HPF):** Corte ajustable (20–300 Hz) para eliminar golpes de mesa y ruidos por debajo del rango vocal.
- **Puerta de Ruido de Estudio (Noise Gate):** Umbral configurable (-60 a 0 dB) con histéresis, attack de 2ms y release suave para silenciar ruidos de fondo sin comerse los finales de frase.
- **Ecualizador Paramétrico de 3 Bandas:** Graves (100 Hz Low Shelf), Medios (1 kHz Peak) y Agudos (8 kHz High Shelf) de -15 a +15 dB.
- **Compresor Dinámico de Estudio:** Control de dinámica vocal con umbral, ratio (1:1 a 10:1), ganancia de compensación (makeup) y medidor de reducción de ganancia.
- **Efectos de Carácter y Modulación:**
  - **Robotizer / Vocoder:** Modulación por tren de pulsos armónicos con frecuencia portadora (20–500 Hz) y mezcla Dry/Wet.
  - **Modulador en Anillo (Ring Modulator):** Portadora senoidal pura (50–2000 Hz) para voces de robots clásicos (Dalek), aliens y modulaciones metálicas.
  - **Distorsión / Overdrive / Bitcrusher:** Saturación suave por tangente hiperbólica y drive para radios policiales, megáfonos e intercomunicadores.
  - **Chorus / Doubler:** Línea de retardo modulada por LFO para engrosar la voz estéreo y dar sensación de coro.
  - **Reverberación de Sala:** Red de filtros en peine y all-pass estéreo para ambientación espacial (cueva, catedral, sala pequeña).
  - **Humanizer:** Micro-deriva orgánica de pitch para evitar el efecto de sintetizador rígido.
- **Limitador de Seguridad:** Protección continua contra clipping y picos digitales imprevistos.

### 🎧 Sidetone ("Escuchar mi voz") y Voice Tester Loop
- **Sidetone de Retorno:** Stream `mvx.monitor` en tiempo real hacia tus auriculares con botón de activación instantánea y ganancia independiente (-24 a +12 dB).
- **Voice Tester Loop:** Grabadora circular de prueba de 8 segundos. Permite grabar una frase corta y escucharla en bucle continuo mientras ajustas los efectos y sliders del Voice Lab sin tener que hablar todo el tiempo.

### 🔊 Soundboard / Sampler Integrado
- Reproducción instantánea de clips de sonido WAV directamente hacia el micrófono virtual y tus auriculares.
- **Zero-Allocation & Zero-Lock:** Mezcla en tiempo real sin mutexes ni asignaciones dinámicas de memoria.
- **Ducking Automático del Micrófono:** El micrófono físico se atenúa automáticamente un 70% (-10 dB) mientras se reproduce un clip de sonido para evitar saturaciones y mejorar la inteligibilidad.
- **Sonidos de Fábrica Sintetizados:** `airhorn`, `bleep`, `chime`, `buzzer` generados automáticamente en `~/.local/share/mvx/sounds/`.
- Permite arrastrar o copiar cualquier archivo WAV estándar a esa carpeta para usarlo de inmediato.

### 🛡️ "Solo mi voz" (Biometría Vocal CAM++ + Silero VAD)
- Verificación de hablante por redes neuronales ONNX fuera del hilo de tiempo real.
- Bloquea voces ajenas de fondo en oficinas abiertas, cibercafés o casas familiares abriendo el micrófono solo cuando habla el usuario enrolado.

### 🎛️ 14 Presets de Fábrica
1. **Natural (Limpio):** Voz transparente con acondicionamiento de estudio (HPF 80Hz + Gate suave).
2. **Locutor Grave (Broadcast):** Tono grave cálido (+compresor + EQ de graves).
3. **Voz de Ardilla (Squirrel):** Pitch y formantes elevados en sincronía (+6 semitonos).
4. **Titán / Demonio:** Pitch ultra-bajo (-5 st) con overdrive y reverberación cavernosa.
5. **Cyborg / Androide:** Robotizador armónico a 90 Hz con ecualización metálica.
6. **Radio Policial (Walkie-Talkie):** HPF a 300Hz, distorsión por recorte y saturación de medios.
7. **Megáfono:** Resonancia en medios con drive y compresión agresiva.
8. **Cueva Eco (Echo Cave):** Sala grande con reflexiones profundas.
9. **Anónimo:** Pitch alterado y formantes ensanchados para ocultar la identidad.
10. **Alien (Dalek):** Modulación en anillo a 280 Hz.
11. **Teléfono Vintage:** Filtro paso banda telefónico estrecho con saturación.
12. **Helio:** Formantes afinados hacia arriba sin alterar excesivamente el tono.
13. **Demonio Radial (Radio Demon):** Overdrive con mezcla de chorus y reverberación oscura.
14. **Robot Musical:** Modulación precisa afinada para gaming y streams.

---

## Arquitectura de Tiempo Real (RT-Safe)

Diseñado estrictamente para Linux y PipeWire:
- **0 Bloqueos en Audio Thread:** Cero mutexes, cero llamadas al sistema, cero syscalls ni esperas dentro de los callbacks de PipeWire.
- **Ring Buffer SPSC Mono:** Transporte eficiente de 1 muestra por frame con expansión multicanal automática a la salida.
- **Gliding Atómico de Parámetros:** Mover sliders no destruye ni recrea nodos DSP. El pitch y los filtros se interpolan suavemente sin clics ni xruns.
- **Flock de Instancia Única:** `mvxd` y `mvx-tray` están protegidos por candados de archivo `flock(2)` para evitar instancias duplicadas.
- **Servicios de Usuario Systemd:** Corren bajo el usuario no privilegiado (`systemctl --user start mvxd`).

---

## Requisitos

- **Linux** con PipeWire 1.2+ y WirePlumber.
- **Rust** 1.80+ y herramientas de compilación (`gcc-libs`, `clang`).
- Opcional: `waybar`, `hyprland`, `sway`, `kde`, `libayatana-appindicator`.

En Arch Linux / CachyOS / Manjaro:
```bash
sudo pacman -S --needed pipewire pipewire-audio wireplumber rust base-devel clang
```

---

## Instalación y Compilación

### Compilar desde la fuente:
```bash
git clone https://github.com/x4yi/ModularVoiceX4yi.git
cd ModularVoiceX4yi
cargo build --release
```

Los binarios generados se encuentran en:
- `target/release/mvxd` — Daemon de audio PipeWire.
- `target/release/mvx` — Interfaz TUI y CLI de control.
- `target/release/mvx-tray` — Icono StatusNotifierItem en la barra del sistema.

### Instalación en Arch Linux (PKGBUILD):
```bash
cd packaging/arch
makepkg -si
```

---

## Uso

### Iniciar la aplicación:
```bash
mvx
```
Al ejecutar `mvx`:
- Si el daemon `mvxd` no está corriendo, se inicia automáticamente en segundo plano.
- Se abre la interfaz interactiva con 4 pestañas: **Estudio**, **Voice Lab**, **Soundboard** y **Dispositivos**.
- Compatible con **ratón** (clic, arrastrar sliders, rueda de desplazamiento) y **teclado**.

### Atajos de teclado en la TUI:
- `1`..`4` o `Tab` — Cambiar de pestaña (`Estudio`, `Voice Lab`, `Soundboard`, `Dispositivos`).
- `m` — Alternar Mute (silenciar micrófono).
- `b` — Alternar Bypass DSP (micrófono limpio directo).
- `s` — Alternar Sidetone / Retorno en auriculares.
- `v` — Alternar "Solo mi voz" (CAM++ Speaker Lock).
- `l` — Alternar grabación / bucle de prueba (Voice Tester Loop).
- `Espacio` — Detener todos los sonidos de la soundboard.
- `1`..`9` (en Soundboard) — Disparar sonido por número de clip.
- `+` / `-` — Subir / bajar volumen maestro.
- `[` / `]` — Modificar tono (pitch).
- `q` o `Esc` — Salir de la TUI (el daemon sigue corriendo sin cortar la voz).

---

## Integración con Waybar y Atajos Globales (CLI)

MVX incluye el subcomando `mvx ctl` para asignar atajos globales en tu gestor de ventanas (Hyprland, Sway, KDE, i3):

```bash
# Alternar mute del micrófono
mvx ctl toggle-mute

# Alternar bypass (para hablar natural al instante)
mvx ctl toggle-bypass

# Alternar monitoreo en auriculares (sidetone)
mvx ctl toggle-monitor

# Ajustar volumen de monitoreo en dB
mvx ctl monitor-gain --gain-db 3.0

# Siguiente o anterior preset
mvx ctl next-preset
mvx ctl prev-preset

# Aplicar preset por ID
mvx ctl preset --id voz_ardilla

# Disparar un sonido de la soundboard
mvx ctl sound --id airhorn

# Detener todos los sonidos
mvx ctl stop-sounds

# Controlar la grabadora de bucle
mvx ctl loop-test
```

### Módulo Waybar (`~/.config/waybar/config.jsonc`):
```jsonc
"custom/mvx": {
    "format": "{}",
    "return-type": "json",
    "exec": "mvx status --waybar",
    "interval": 2,
    "on-click": "mvx ctl toggle-mute",
    "on-click-right": "mvx ctl toggle-bypass",
    "on-click-middle": "mvx ctl next-preset"
}
```

---

## Diagnóstico y Salud

Para verificar el estado de PipeWire, el quantum, la latencia total y la ruta de audio:
```bash
mvx doctor
```

Para consultar los modelos biométricos de voz:
```bash
mvx models install   # Descarga CAM++ y Silero VAD (~30 MB)
mvx models status    # Muestra estado de archivos y perfiles
```

---

## Licencia

Distribuido bajo la licencia [MIT](LICENSE).
Diseñado exclusivamente para el ecosistema moderno de audio en GNU/Linux.
