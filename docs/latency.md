# Latencia

## Objetivo

- Perfil normal: quantum **256** a 48 kHz → 5.33 ms por bloque.
- Perfil ultra: quantum **128** → 2.67 ms por bloque.
- `Natural` no agrega DSP: la latencia es la del puente PipeWire.
- `Solo mi voz` no agrega latencia de audio con `lookahead_ms = 0`; el análisis corre en paralelo en otro hilo.

El daemon crea el nodo virtual con:

```text
node.force-quantum = <quantum>
node.lock-quantum  = true
```

Mientras `mvxd` corre, ese nodo actúa como driver del grafo y fija el quantum. Es una decisión deliberada de baja latencia; también puede afectar a otros nodos de la sesión.

## Presupuesto de latencia

La TUI muestra:

```text
latencia base + DSP = total
```

- **Base**: ring de salida + un quantum. En reposo, el ring se mantiene en ~2 bloques; con quantum 256 suele dar ~16 ms.
- **DSP**: suma de las etapas activas.

| Etapa | Latencia |
|---|---:|
| Signalsmith Ultra (1024) | 21.33 ms |
| Signalsmith Equilibrado (2048) | 42.67 ms |
| Signalsmith Alta (4096) | 85.33 ms |
| DeepFilterNet3 | 30 ms |
| Compuerta de hablante (`lookahead_ms`) | 0–300 ms |
| Limitador | 0 ms |
| Ganancia / mute | 0 ms |

Un preset con pitch/formantes y NR en calidad Equilibrada queda en ~52.7 ms de DSP. `Natural` queda en 0 ms de DSP.

`lookahead_ms` está en 0 por defecto. Aumentarlo retrasa el audio para que la decisión de la compuerta se aplique a muestras anteriores; es útil solo si preferís menos fugas después de una decisión y aceptás más latencia. No acelera la detección: la ventana de embedding sigue siendo de ~0,5 s.

## Pre-roll

El ring se llena inicialmente antes de que el playback empiece. Para no heredar todo ese buffer como latencia:

- se mantiene un pre-roll de **2 bloques**;
- si el ring supera **4 bloques**, se descarta el exceso hasta volver a 2;
- el objetivo es ~10.7 ms de buffer interno con quantum 256, más el quantum y la latencia del hardware.

La latencia mostrada es una estimación (frames en el ring + quantum + DSP), no una medición end-to-end.

## Reconstrucción del camino

Cambiar pitch o formantes mientras el preset ya usa Stretch actualiza el parámetro en el callback sin reconstruir. Pasar de un preset sin pitch a uno con pitch (o viceversa), cambiar la calidad DSP, encender/apagar NR o Solo mi voz, cambiar el perfil de voz o el lookahead sí reconstruye brevemente los streams de captura/playback. Durante esa transición pueden aparecer algunos `underruns`; no son fallos del estado estable. Cambiar ganancia, mute, intensidad de NR, umbral o hold no reconstruye el camino; el worker lee umbral y hold en cada análisis.

## Medición

Para inspeccionar el grafo:

```bash
pw-top
pw-cli info mvx_mic
pw-cli info mvx.output
pw-link -l
```

Contadores en la TUI/doctor:

- `xruns`: fallos de dequeue del stream.
- `underruns`: bloques de playback sin muestras suficientes.
- `overflows`: ring lleno en captura.

Con el estado actual el ring debe quedar estable en 1024 muestras (2 bloques) y los contadores no deben crecer en reposo.

## Deriva de reloj

La captura sigue el reloj del dispositivo físico; el playback sigue el nodo virtual. Si sus relojes derivan, el ring puede llenarse o vaciarse. La implementación actual acota el problema con pre-roll y descarte de exceso. La solución definitiva es un resampler adaptativo controlado por el nivel del ring.
