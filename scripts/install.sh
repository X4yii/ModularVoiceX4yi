#!/usr/bin/env bash
# Instalador de MVX (ModularVoiceX4yi) para Linux con PipeWire.
# Proyecto desarrollado con asistencia de IA. Uso bajo propio riesgo.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
MIN_RUST="1.96"
MIN_RAM_MB=3500
MIN_DISK_MB=3000

PREFIX="${PREFIX:-$HOME/.local}"
ASSUME_YES=0
DRY_RUN=0
NO_DEPS=0
NO_MODELS=0
NO_CLONE_MODELS=0
NO_SYSTEMD=0
FAST=0

LOG_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/mvx"
LOG_FILE="$LOG_DIR/install.log"

usage() {
    cat <<'EOF'
Uso: ./scripts/install.sh [opciones]

  --prefix DIR         Prefijo de instalacion (por defecto ~/.local)
  --yes, -y            No preguntar (acepta instalar dependencias con sudo)
  --dry-run            Mostrar lo que se haria sin ejecutar nada
  --no-deps            No instalar dependencias del sistema
  --no-models          No descargar ningun modelo
  --no-clone-models    No descargar modelos de clonacion de voz (ALPHA, ~160 MB)
  --no-systemd         No instalar unidades systemd de usuario
  --fast               Compilar sin LTO (mas rapido, binario algo menos optimizado)
  --uninstall          Desinstalar (ver scripts/uninstall.sh)
  --help, -h           Mostrar esta ayuda
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="${2:?falta valor para --prefix}"; shift ;;
        --yes|-y) ASSUME_YES=1 ;;
        --dry-run) DRY_RUN=1 ;;
        --no-deps) NO_DEPS=1 ;;
        --no-models) NO_MODELS=1 ;;
        --no-clone-models) NO_CLONE_MODELS=1 ;;
        --no-systemd) NO_SYSTEMD=1 ;;
        --fast) FAST=1 ;;
        --uninstall) exec "$ROOT/scripts/uninstall.sh" "${@:2}" ;;
        --help|-h) usage; exit 0 ;;
        *) echo "Opcion desconocida: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

BIN="$PREFIX/bin"
SYSTEMD_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICONS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"

mkdir -p "$LOG_DIR"
if [ "$DRY_RUN" -eq 0 ]; then
    exec > >(tee -a "$LOG_FILE") 2>&1
fi

info() { printf '==> %s\n' "$*"; }
warn() { printf 'AVISO: %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; echo "Registro: $LOG_FILE" >&2; exit 1; }
run()  {
    if [ "$DRY_RUN" -eq 1 ]; then
        printf '[dry-run] %s\n' "$*"
    else
        "$@"
    fi
}
have() { command -v "$1" >/dev/null 2>&1; }

ver_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]; }

cat <<'EOF'
MVX - Modulador de voz para Linux (PipeWire)
Proyecto hecho con asistencia de IA. La clonacion de voz es ALPHA (etapa muy temprana).

EOF

# ---------------------------------------------------------------- requisitos
info "Verificando requisitos del entorno"

[ "$(uname -s)" = "Linux" ] || die "MVX solo soporta Linux."

ARCH="$(uname -m)"
if [ "$ARCH" != "x86_64" ]; then
    warn "Arquitectura $ARCH: solo x86_64 esta probada. La inferencia neuronal puede ser lenta."
fi
if [ "$ARCH" = "x86_64" ] && ! grep -qw avx2 /proc/cpuinfo 2>/dev/null; then
    warn "La CPU no reporta AVX2: el rendimiento de los modelos neuronales sera bajo."
fi

if [ "$(id -u)" -eq 0 ]; then
    die "No ejecutes este script como root; instala por usuario (se pedira sudo solo para dependencias)."
fi

RAM_MB="$(awk '/MemTotal/ {printf "%d", $2/1024}' /proc/meminfo 2>/dev/null || echo 0)"
if [ "${RAM_MB:-0}" -lt "$MIN_RAM_MB" ]; then
    warn "RAM total ${RAM_MB} MB (< ${MIN_RAM_MB} MB): la compilacion puede fallar. Prueba --fast."
fi

mkdir -p "$PREFIX" 2>/dev/null || true
DISK_MB="$(df -Pm "$ROOT" 2>/dev/null | awk 'NR==2 {print $4}')"
if [ "${DISK_MB:-0}" -lt "$MIN_DISK_MB" ]; then
    warn "Espacio libre ${DISK_MB} MB (< ${MIN_DISK_MB} MB) en el directorio del proyecto."
fi

# ---------------------------------------------------------------- dependencias
PKG_MGR=""
OS_ID=""
OS_LIKE=""
if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    OS_ID="${ID:-}"
    OS_LIKE="${ID_LIKE:-}"
fi
case " $OS_ID $OS_LIKE " in
    *" arch "*|*" manjaro "*) PKG_MGR=pacman ;;
    *" debian "*|*" ubuntu "*) PKG_MGR=apt ;;
    *" fedora "*|*" rhel "*|*" centos "*) PKG_MGR=dnf ;;
    *" suse "*|*" opensuse "*) PKG_MGR=zypper ;;
    *" void "*) PKG_MGR=xbps ;;
    *" alpine "*) PKG_MGR=apk ;;
esac

declare -a PKGS=()
case "$PKG_MGR" in
    pacman) PKGS=(base-devel clang cmake pkgconf pipewire pipewire-pulse wireplumber ffmpeg curl git) ;;
    apt)    PKGS=(build-essential clang cmake pkg-config libpipewire-0.3-dev libspa-0.2-dev libclang-dev pipewire wireplumber ffmpeg curl git ca-certificates) ;;
    dnf)    PKGS=(gcc gcc-c++ clang cmake pkgconf-pkg-config pipewire-devel pipewire wireplumber ffmpeg-free curl git) ;;
    zypper) PKGS=(gcc gcc-c++ clang cmake pkg-config pipewire-devel pipewire wireplumber ffmpeg curl git) ;;
    xbps)   PKGS=(base-devel clang cmake pkg-config pipewire-devel pipewire wireplumber ffmpeg curl git) ;;
    apk)    PKGS=(build-base clang cmake pkgconf pipewire-dev pipewire wireplumber ffmpeg curl git) ;;
esac

missing_requirements() {
    local m=()
    have cc || have gcc || m+=("compilador C (gcc)")
    have clang || m+=("clang")
    have cmake || m+=("cmake")
    have pkg-config || have pkgconf || m+=("pkg-config")
    have curl || have wget || m+=("curl o wget")
    have git || m+=("git")
    have ffmpeg || m+=("ffmpeg")
    if have pkg-config; then
        pkg-config --exists libpipewire-0.3 2>/dev/null || m+=("headers de libpipewire-0.3")
        pkg-config --exists libspa-0.2 2>/dev/null || m+=("headers de libspa-0.2")
    else
        m+=("headers de libpipewire-0.3 / libspa-0.2")
    fi
    printf '%s\n' "${m[@]:-}" | sed '/^$/d'
}

if [ "$NO_DEPS" -eq 0 ]; then
    MISSING="$(missing_requirements || true)"
    if [ -n "$MISSING" ]; then
        info "Faltan dependencias:"
        while IFS= read -r line; do printf '  - %s\n' "$line"; done <<<"$MISSING"
        if [ -z "$PKG_MGR" ]; then
            warn "Distribucion no reconocida (ID=${OS_ID:-?}). Instala manualmente: compilador C, clang, cmake, pkg-config,"
            warn "headers de libpipewire-0.3 y libspa-0.2, ffmpeg, curl/wget y git. Luego ejecuta con --no-deps."
            [ "$DRY_RUN" -eq 1 ] || die "Dependencias faltantes."
        else
            REPLY_OK=0
            if [ "$ASSUME_YES" -eq 1 ]; then
                REPLY_OK=1
            elif [ -t 0 ]; then
                read -r -p "Instalar con $PKG_MGR (requiere sudo): ${PKGS[*]} ? [s/N] " ans
                case "$ans" in s|S|y|Y) REPLY_OK=1 ;; esac
            fi
            if [ "$REPLY_OK" -eq 1 ]; then
                have sudo || [ "$DRY_RUN" -eq 1 ] || die "sudo no esta disponible; instala las dependencias manualmente."
                info "Instalando dependencias ($PKG_MGR)"
                case "$PKG_MGR" in
                    pacman) run sudo pacman -S --needed --noconfirm "${PKGS[@]}" ;;
                    apt)    run sudo apt-get update; run sudo apt-get install -y "${PKGS[@]}" ;;
                    dnf)    run sudo dnf install -y "${PKGS[@]}" ;;
                    zypper) run sudo zypper --non-interactive install "${PKGS[@]}" ;;
                    xbps)   run sudo xbps-install -Sy "${PKGS[@]}" ;;
                    apk)    run sudo apk add "${PKGS[@]}" ;;
                esac
            else
                [ "$DRY_RUN" -eq 1 ] || die "Instala las dependencias indicadas (o usa --yes) y vuelve a ejecutar."
            fi
        fi
    else
        info "Dependencias del sistema: OK"
    fi
fi

# ---------------------------------------------------------------- PipeWire
if have pw-cli && pw-cli info >/dev/null 2>&1; then
    info "PipeWire: activo"
else
    warn "PipeWire no responde. MVX lo requiere (junto con WirePlumber). Inicia tu sesion con PipeWire antes de usar mvx."
fi

# ---------------------------------------------------------------- Rust
RUST_OK=0
if have cargo && have rustc && rustc --version >/dev/null 2>&1; then
    CUR="$(rustc --version | awk '{print $2}')"
    if ver_ge "$CUR" "$MIN_RUST"; then
        RUST_OK=1
        info "Rust $CUR: OK"
    else
        warn "Rust $CUR es anterior al minimo requerido ($MIN_RUST)."
    fi
fi
if [ "$RUST_OK" -eq 0 ]; then
    if have rustup; then
        info "Actualizando toolchain con rustup"
        run rustup update stable || die "rustup update fallo."
    else
        if [ "$ASSUME_YES" -eq 0 ] && [ -t 0 ] && [ "$DRY_RUN" -eq 0 ]; then
            read -r -p "Instalar Rust (rustup) en ~/.cargo ? [s/N] " ans
            case "$ans" in s|S|y|Y) ;; *) die "Rust >= $MIN_RUST es requerido." ;; esac
        fi
        have curl || [ "$DRY_RUN" -eq 1 ] || die "curl es necesario para instalar rustup."
        info "Instalando Rust con rustup"
        if [ "$DRY_RUN" -eq 1 ]; then
            echo "[dry-run] curl https://sh.rustup.rs | sh -s -- -y --profile minimal"
        else
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        fi
    fi
    # shellcheck disable=SC1091
    [ -r "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
fi

# ---------------------------------------------------------------- compilacion
info "Compilando MVX (release)"
CARGO_ENV=()
if [ "$FAST" -eq 1 ]; then
    CARGO_ENV=(CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16)
fi
if [ "$DRY_RUN" -eq 1 ]; then
    echo "[dry-run] cargo build --release --locked --manifest-path $ROOT/Cargo.toml"
else
    env ${CARGO_ENV[@]+"${CARGO_ENV[@]}"} cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" \
        || die "La compilacion fallo. Revisa las dependencias (libpipewire, clang) y la conexion de red."
fi

# ---------------------------------------------------------------- instalacion
info "Instalando binarios en $BIN"
run install -d "$BIN"
for b in mvxd mvx mvx-tray; do
    run install -m755 "$ROOT/target/release/$b" "$BIN/$b"
done
run ln -sfn mvx "$BIN/mxv"

# Asegurar que $BIN este en PATH en el perfil de shell del usuario
ensure_path() {
    case ":$PATH:" in
        *":$BIN:"*) return 0 ;;
    esac
    info "Configurando $BIN en el PATH del usuario"
    local line="export PATH=\"$BIN:\$PATH\""
    for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
        if [ -f "$rc" ] && ! grep -qF "$BIN" "$rc" 2>/dev/null; then
            if [ "$DRY_RUN" -eq 1 ]; then
                echo "[dry-run] agregar '$line' en $rc"
            else
                printf '\n# MVX\n%s\n' "$line" >> "$rc"
                info "Agregado a $rc"
            fi
        fi
    done
    local fish_cfg="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
    if [ -f "$fish_cfg" ] && ! grep -qF "$BIN" "$fish_cfg" 2>/dev/null; then
        if [ "$DRY_RUN" -eq 1 ]; then
            echo "[dry-run] agregar fish_add_path $BIN en $fish_cfg"
        else
            printf '\n# MVX\nfish_add_path %s\n' "$BIN" >> "$fish_cfg"
            info "Agregado a $fish_cfg"
        fi
    fi
}
ensure_path

if [ "$NO_SYSTEMD" -eq 0 ] && have systemctl; then
    info "Instalando unidades systemd de usuario"
    run install -d "$SYSTEMD_DIR"
    for u in mvxd mvx-tray; do
        if [ "$DRY_RUN" -eq 1 ]; then
            echo "[dry-run] generar $SYSTEMD_DIR/$u.service con ExecStart=$BIN/$u"
        else
            sed "s|^ExecStart=.*|ExecStart=$BIN/$u|" "$ROOT/packaging/systemd/$u.service" > "$SYSTEMD_DIR/$u.service"
        fi
    done
    run systemctl --user daemon-reload || warn "No se pudo recargar systemd de usuario (sesion sin bus?)."
elif [ "$NO_SYSTEMD" -eq 0 ]; then
    warn "systemd no disponible: inicia el daemon con 'mvx' (se lanza automaticamente)."
fi

info "Instalando lanzador de escritorio"
run install -d "$APPS_DIR" "$ICONS_DIR"
run install -m644 "$ROOT/packaging/desktop/modularvoicex4yi.desktop" "$APPS_DIR/modularvoicex4yi.desktop"
run install -m644 "$ROOT/packaging/desktop/modularvoicex4yi.svg" "$ICONS_DIR/modularvoicex4yi.svg"
if have update-desktop-database; then run update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true; fi

# ---------------------------------------------------------------- modelos
if [ "$NO_MODELS" -eq 0 ]; then
    info "Descargando modelos neuronales"
    if [ "$NO_CLONE_MODELS" -eq 1 ]; then
        export MVX_SKIP_CLONE_MODELS=1
    else
        warn "Se descargan los modelos de clonacion de voz (ALPHA, ~160 MB)."
    fi
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "[dry-run] $BIN/mvx models install"
    else
        # La descarga corre en el daemon: reiniciarlo para que herede el entorno.
        PATH="$BIN:$PATH" "$BIN/mvx" shutdown >/dev/null 2>&1 || true
        sleep 1
        PATH="$BIN:$PATH" "$BIN/mvx" models install \
            || warn "Descarga de modelos fallida; reintenta luego con 'mvx models install'."
        info "Esperando la descarga de modelos (hasta 10 min)"
        for _ in $(seq 1 300); do
            PENDING="$(PATH="$BIN:$PATH" "$BIN/mvx" models status 2>/dev/null | grep '^\[!!\]' || true)"
            if [ "$NO_CLONE_MODELS" -eq 1 ]; then
                PENDING="$(printf '%s\n' "$PENDING" | grep -v OpenVoice || true)"
            fi
            [ -z "$PENDING" ] && break
            sleep 2
        done
        [ -z "${PENDING:-}" ] || warn "Modelos pendientes; revisa 'mvx models status' y tu conexion."
    fi
fi

# ---------------------------------------------------------------- verificacion
info "Verificando instalacion"
if [ "$DRY_RUN" -eq 0 ]; then
    PATH="$BIN:$PATH" "$BIN/mvx" doctor || warn "mvx doctor reporto problemas."
fi

cat <<EOF

Instalacion terminada.

  mvx                      # abre la TUI (inicia el daemon si hace falta)
  mvx doctor               # diagnostico del entorno
  mvx models status        # estado de modelos
  mvx clone <audio>        # [ALPHA] clonar una voz

Selecciona "MVX Microphone" (mvx_mic) como entrada en Discord, OBS, etc.

Opcional (arranque automatico en cada sesion):
  systemctl --user enable --now mvxd.service mvx-tray.service
EOF
