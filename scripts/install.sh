#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin"
SYSTEMD="$HOME/.config/systemd/user"
APPLICATIONS="$HOME/.local/share/applications"

echo "==> Compilando MVX (release)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"

echo "==> Instalando binarios en $BIN"
install -d "$BIN"
install -m755 "$ROOT/target/release/mvxd" "$BIN/mvxd"
install -m755 "$ROOT/target/release/mvx" "$BIN/mvx"
install -m755 "$ROOT/target/release/mvx-tray" "$BIN/mvx-tray"
# Alias solicitado: `mxv` es exactamente el mismo cliente que `mvx`.
ln -sfn mvx "$BIN/mxv"

echo "==> Instalando unidades systemd de usuario"
install -d "$SYSTEMD"
install -m644 "$ROOT/packaging/systemd/mvxd.service" "$SYSTEMD/mvxd.service"
install -m644 "$ROOT/packaging/systemd/mvx-tray.service" "$SYSTEMD/mvx-tray.service"

echo "==> Instalando lanzador"
install -d "$APPLICATIONS"
install -m644 "$ROOT/packaging/desktop/mvx.desktop" "$APPLICATIONS/mvx.desktop"

cat <<'MSG'

Instalado.

  mxv          # TUI + daemon + tray (también disponible como `mvx`)
  mxv doctor
  mxv status

Si querés que el daemon y el tray sobrevivan reinicios de sesión:

  systemctl --user daemon-reload
  systemctl --user enable --now mvxd.service
  systemctl --user enable --now mvx-tray.service
MSG

case ":$PATH:" in
    *":$BIN:"*) ;;
    *) echo "AVISO: $BIN no está en PATH; agregalo para ejecutar 'mxv'." ;;
esac
