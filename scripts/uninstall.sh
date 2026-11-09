#!/usr/bin/env bash
# Desinstalador de MVX. Conserva configuracion y datos salvo con --purge.
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
PURGE=0
for arg in "$@"; do
    case "$arg" in
        --purge) PURGE=1 ;;
        --prefix=*) PREFIX="${arg#*=}" ;;
        --help|-h)
            echo "Uso: scripts/uninstall.sh [--purge] [--prefix=DIR]"
            echo "  --purge  Borra tambien ~/.config/mvx y ~/.local/share/mvx (modelos, voces, presets)."
            exit 0 ;;
        *) echo "Opcion desconocida: $arg" >&2; exit 2 ;;
    esac
done

BIN="$PREFIX/bin"
CFG="${XDG_CONFIG_HOME:-$HOME/.config}"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"

echo "==> Deteniendo servicios"
if command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable --now mvx-tray.service mvxd.service >/dev/null 2>&1 || true
fi
if [ -x "$BIN/mvx" ]; then "$BIN/mvx" shutdown >/dev/null 2>&1 || true; fi
pkill -x mvx-tray 2>/dev/null || true

echo "==> Eliminando archivos instalados"
rm -f "$BIN/mvx" "$BIN/mvxd" "$BIN/mvx-tray" "$BIN/mxv"
rm -f "$CFG/systemd/user/mvxd.service" "$CFG/systemd/user/mvx-tray.service"
rm -f "$DATA/applications/modularvoicex4yi.desktop"
rm -f "$DATA/icons/hicolor/scalable/apps/modularvoicex4yi.svg"
command -v systemctl >/dev/null 2>&1 && systemctl --user daemon-reload >/dev/null 2>&1 || true

if [ "$PURGE" -eq 1 ]; then
    echo "==> Purgando configuracion y datos"
    rm -rf "$CFG/mvx" "$DATA/mvx" "${XDG_CACHE_HOME:-$HOME/.cache}/mvx"
else
    echo "Se conservan $CFG/mvx y $DATA/mvx (usa --purge para borrarlos)."
fi
echo "Desinstalado."

