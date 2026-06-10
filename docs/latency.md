# Latencia

## Objetivo

- Perfil normal: quantum **256** a 48 kHz → 5.33 ms por bloque.
- Perfil ultra: quantum **128** → 2.67 ms por bloque.

El daemon crea el nodo virtual con:

```text
node.force-quantum = <quantum>
node.lock-quantum  = true
```

Mientras `mvxd` corre, ese nodo actúa como driver del grafo y fija el quantum. Es una decisión deliberada de baja latencia; también puede afectar a otros nodos de la sesión. En el futuro será configurable por perfil y se podrá desactivar.

## Pre-roll

El ring se llena inicialmente antes de que el playback empiece. Para no heredar todo ese buffer como latencia:

- se mantiene un pre-roll de **2 bloques**;
- si el ring supera **4 bloques**, se descarta el exceso hasta volver a 2;
- el objetivo es ~10.7 ms de buffer interno con quantum 256, más el quantum y la latencia del hardware.

La TUI muestra `latency_ms` como estimación: frames en el ring + un quantum. No es una medición end-to-end.

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

La captura sigue el reloj del dispositivo físico; el playback sigue el nodo virtual. Si sus relojes derivan, el ring puede llenarse o vaciarse. M0 acota el problema con pre-roll y descarte de exceso. La solución definitiva es un resampler adaptativo controlado por el nivel del ring.
