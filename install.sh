#!/usr/bin/env bash
#
# noaa-recon-api installer / updater / uninstaller.
#
# Quick install (as your normal user, not root):
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/jjmurdock19/noaa-recon-api/main/install.sh)"
#
# (Deliberately NOT `curl ... | bash` — piping straight into bash hands bash
# its own script over a live pipe, and this script needs real keyboard input
# on stdin for its prompts. `bash -c "$(curl ...)"` downloads the whole
# script into memory first via command substitution, so bash's stdin stays
# the real terminal throughout and the wizard can read from it normally.)
#
# Or clone the repo first and run it locally:
#   git clone --recurse-submodules https://github.com/jjmurdock19/noaa-recon-api.git
#   cd noaa-recon-api && ./install.sh
#
# Re-running this script on a machine that already has noaa-recon-api
# installed offers Update / Reconfigure / Uninstall instead of installing
# again. See INSTALL.md for the plain-language walkthrough of every
# question this script asks.
#
set -euo pipefail

if [[ ! -t 0 ]]; then
    printf 'Heads up: stdin isn'"'"'t a terminal, so the prompts below will\n' >&2
    printf 'silently accept their defaults instead of waiting for input.\n' >&2
    printf 'For an interactive install, use:\n' >&2
    printf '  bash -c "$(curl -fsSL %s)"\n\n' \
        "https://raw.githubusercontent.com/jjmurdock19/noaa-recon-api/main/install.sh" >&2
fi

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
REPO_URL="https://github.com/jjmurdock19/noaa-recon-api.git"
SERVICE_NAME="noaa-recon-api"
CONFIG_FILE="/etc/noaa-recon-api/install.conf"
CLI_PATH="/usr/local/bin/noaa-recon-api"

# Wizard defaults — overridden by an existing $CONFIG_FILE when reconfiguring/updating.
INSTALL_DIR="/opt/noaa-recon-api"
RUN_USER=""
BRANCH="main"
# Which implementation to run as the API server: "python" (FastAPI, branch main)
# or "rust" (compiled axum server, branch rust). Empty => the version picker asks.
# NOTE: the rust variant is fully self-contained (ingest + cache-cleanup are
# native subcommands, see main.rs) and never gets a Python venv — see
# configure_admin_credentials()/configure_auth() below, which branch on this
# to avoid touching .venv/bin/python3 for that variant.
VARIANT=""
PORT="8000"
NET_MODE="local"      # local | lan | domain
DOMAIN=""
API_PATH="/api"
DOMAIN_MODE="subdomain" # subdomain | path
WEBSERVER="none"        # none | nginx | apache
HTTPS_ENABLED=""
ADMIN_USER=""
ADMIN_PASS=""
AUTH_ENABLED="n"   # y | n — require API tokens for the public /v1/* data endpoints

# ---------------------------------------------------------------------------
# UI helpers — everything here writes prompts/decoration to stderr so that
# `x=$(ask_text ...)` only ever captures the actual answer.
# ---------------------------------------------------------------------------
c_reset=$'\033[0m'; c_bold=$'\033[1m'; c_dim=$'\033[2m'
c_red=$'\033[1;31m'; c_green=$'\033[1;32m'; c_yellow=$'\033[1;33m'; c_cyan=$'\033[1;36m'

log_step() { printf "\n${c_bold}${c_cyan}==>${c_reset} ${c_bold}%s${c_reset}\n" "$1" >&2; }
log_ok()   { printf "${c_green}  ok${c_reset}  %s\n" "$1" >&2; }
log_warn() { printf "${c_yellow}  !!${c_reset}  %s\n" "$1" >&2; }
log_err()  { printf "${c_red}  xx${c_reset}  %s\n" "$1" >&2; }
die()      { log_err "$1"; exit 1; }

ask_text() {
    local prompt="$1" default="$2" ans
    if [[ ! -t 0 ]]; then printf '%s\n' "$default"; return; fi
    printf "  %s${default:+ [${c_dim}%s${c_reset}]}: " "$prompt" "$default" >&2
    IFS= read -r ans || true
    printf '%s\n' "${ans:-$default}"
}

ask_yesno() {
    local prompt="$1" default="${2:-y}" ans hint
    [[ "$default" == "y" ]] && hint="Y/n" || hint="y/N"
    if [[ ! -t 0 ]]; then [[ "$default" == "y" ]]; return; fi
    while true; do
        printf "  %s [%s] " "$prompt" "$hint" >&2
        IFS= read -r ans || true
        ans="${ans:-$default}"
        case "$ans" in
            [Yy]*) return 0 ;;
            [Nn]*) return 1 ;;
            *) printf "  please answer y or n\n" >&2 ;;
        esac
    done
}

# menu_select "Prompt" "Option A" "Option B" ... -> sets MENU_RESULT, MENU_INDEX
menu_select() {
    local prompt="$1"; shift
    local options=("$@")
    local count=${#options[@]}
    local selected=0 key rest

    if [[ ! -t 0 ]]; then
        MENU_RESULT="${options[0]}"; MENU_INDEX=0
        printf "  %s -> %s (non-interactive, using default)\n" "$prompt" "${options[0]}" >&2
        return
    fi

    _menu_draw() {
        local i
        for i in "${!options[@]}"; do
            if [[ $i -eq $selected ]]; then
                printf "\r\033[K  ${c_cyan}> %s${c_reset}\n" "${options[$i]}" >&2
            else
                printf "\r\033[K    %s\n" "${options[$i]}" >&2
            fi
        done
    }

    printf "\n${c_bold}%s${c_reset}\n" "$prompt" >&2
    printf "  (arrow keys or j/k to move, Enter to choose)\n" >&2
    _menu_draw
    tput civis 2>/dev/null || true
    while true; do
        IFS= read -rsn1 key
        if [[ $key == $'\x1b' ]]; then
            IFS= read -rsn2 -t 0.05 rest || true
            key+="$rest"
        fi
        case "$key" in
            $'\x1b[A'|k) selected=$(( (selected - 1 + count) % count )) ;;
            $'\x1b[B'|j) selected=$(( (selected + 1) % count )) ;;
            "") break ;;
        esac
        printf "\033[%dA" "$count" >&2
        _menu_draw
    done
    tput cnorm 2>/dev/null || true
    unset -f _menu_draw
    MENU_RESULT="${options[$selected]}"
    MENU_INDEX=$selected
}

print_banner() {
    # "API" in large block letters, printed inline beside the title/tagline
    # rather than as its own full-width splash.
    local -a logo_lines=(
        ""
        "    ▄▄     ▄▄▄▄▄▄     ▄▄▄▄▄▄  "
        "   ████    ██▀▀▀▀█▄   ▀▀██▀▀  "
        "   ████    ██    ██     ██    "
        "  ██  ██   ██████▀      ██    "
        "  ██████   ██           ██    "
        " ▄██  ██▄  ██         ▄▄██▄▄  "
        " ▀▀    ▀▀  ▀▀         ▀▀▀▀▀▀  "
    )
    local i
    for i in "${!logo_lines[@]}"; do
        logo_lines[$i]="${c_bold}${c_cyan}${logo_lines[$i]}${c_reset}"
    done

    local -a text_lines=(
        "${c_bold}${c_cyan}noaa-recon-api${c_reset} installer"
        ""
        "${c_dim}Open-source API for archival GOES satellite imagery,${c_reset}"
        "${c_dim}NOAA Tail Doppler Radar, and hurricane hunter recon data.${c_reset}"
        ""
        "${c_dim}github.com/jjmurdock19/noaa-recon-api${c_reset}"
    )

    local show_logo=1
    [[ "$(tput cols 2>/dev/null || echo 80)" -lt 82 ]] && show_logo=0

    echo >&2
    local n=${#logo_lines[@]}
    [[ ${#text_lines[@]} -gt $n ]] && n=${#text_lines[@]}
    local blank_logo_line
    blank_logo_line="$(printf '%*s' 31 '')"
    for ((i = 0; i < n; i++)); do
        if [[ "$show_logo" -eq 1 ]]; then
            printf "  %s  %s\n" "${logo_lines[$i]:-$blank_logo_line}" "${text_lines[$i]:-}" >&2
        elif [[ -n "${text_lines[$i]:-}" ]]; then
            printf "  %s\n" "${text_lines[$i]}" >&2
        fi
    done
    echo >&2
}

print_help() {
    cat <<HELP
noaa-recon-api installer

Usage:
  ./install.sh                 Interactive install / update / reconfigure wizard
                               (asks which version to install: Rust or Python)
  ./install.sh --update        Non-interactive: pull latest code, rebuild, restart
  ./install.sh --uninstall     Remove the service, timers, webserver config, and CLI
  ./install.sh --status        Show service status and a health check
  ./install.sh --dir PATH      Install to PATH instead of /opt/noaa-recon-api
  ./install.sh --variant V     'python' (FastAPI, branch main) or 'rust' (compiled
                               axum server, branch rust). Omit to be asked.
  ./install.sh --branch NAME   Track a branch other than the variant's default
  ./install.sh -y, --yes       Accept defaults for anything not given on the command line
  ./install.sh -h, --help      This message

See INSTALL.md in this repo for a plain-language walkthrough.
HELP
}

# ---------------------------------------------------------------------------
# Privilege / package-manager plumbing
# ---------------------------------------------------------------------------
SUDO="sudo"
[[ "$(id -u)" -eq 0 ]] && SUDO=""

run_as() { # run_as USER CMD...
    local u="$1"; shift
    if [[ "$(id -un)" == "$u" ]]; then "$@"; else sudo -u "$u" "$@"; fi
}

# Coreutils-only random token generators, for the rust variant which has no
# Python (venv or system) available — see the VARIANT note above.
gen_token_urlsafe() { # gen_token_urlsafe NUM_BYTES -- like Python's secrets.token_urlsafe()
    head -c "$1" /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n'
}
gen_token_hex() { # gen_token_hex NUM_BYTES -- like Python's secrets.token_hex()
    head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'
}
json_escape() { # json_escape STR -- backslash+quote escaping for embedding in a JSON string literal
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '%s' "$s"
}

detect_pkg_manager() {
    if command -v dnf >/dev/null 2>&1; then echo dnf
    elif command -v apt-get >/dev/null 2>&1; then echo apt
    elif command -v nix-env >/dev/null 2>&1; then echo nix
    else echo unknown
    fi
}

install_base_packages() {
    # The rust variant is fully self-contained (ingest is native subcommands), so
    # it needs NO Python — just git, a C compiler + make (for the netcdf crate's C
    # deps), curl (rustup), and cmake (netCDF-C/HDF5 from source). The python
    # variant needs python3 + venv/pip instead of cmake/curl.
    log_step "Installing base dependencies for the ${VARIANT} variant"
    case "$PKG_MANAGER" in
        dnf)  $SUDO dnf install -y git gcc gcc-c++ make sudo >&2
              if [[ "$VARIANT" == "rust" ]]; then $SUDO dnf install -y cmake curl >&2
              else $SUDO dnf install -y python3 python3-pip python3-devel >&2; fi ;;
        apt)  $SUDO apt-get update -y >&2 && $SUDO apt-get install -y git build-essential sudo >&2
              if [[ "$VARIANT" == "rust" ]]; then $SUDO apt-get install -y cmake curl >&2
              else $SUDO apt-get install -y python3 python3-venv python3-pip python3-dev >&2; fi ;;
        nix)  nix-env -iA nixpkgs.git nixpkgs.gcc nixpkgs.gnumake >&2
              if [[ "$VARIANT" == "rust" ]]; then nix-env -iA nixpkgs.cmake nixpkgs.curl nixpkgs.rustup >&2
              else nix-env -iA nixpkgs.python3 >&2; fi ;;
        *)    log_warn "Unrecognized package manager — ensure git + a C compiler$([[ "$VARIANT" == "rust" ]] && echo " + cmake + curl" || echo " + python3 (venv+pip)") are installed." ;;
    esac
    log_ok "base packages present"
}

install_webserver_package() {
    local ws="$1" svc=""
    case "${PKG_MANAGER}:${ws}" in
        dnf:nginx)  $SUDO dnf install -y nginx >&2;  svc=nginx ;;
        dnf:apache) $SUDO dnf install -y httpd >&2;  svc=httpd ;;
        apt:nginx)  $SUDO apt-get install -y nginx >&2;   svc=nginx ;;
        apt:apache) $SUDO apt-get install -y apache2 >&2; svc=apache2 ;;
        nix:nginx)  nix-env -iA nixpkgs.nginx >&2 ;;
        nix:apache) nix-env -iA nixpkgs.apacheHttpd >&2 ;;
        *) log_warn "Please install $ws manually for this package manager." ;;
    esac
    # Nix doesn't manage services via systemd units the way dnf/apt packages do,
    # so there's nothing to enable there — the operator wires that up themselves.
    [[ -n "$svc" ]] && { $SUDO systemctl enable --now "$svc" 2>/dev/null || log_warn "Couldn't auto-start $svc — start it manually."; }
}

detect_webserver() {
    if command -v nginx >/dev/null 2>&1 || systemctl list-unit-files 2>/dev/null | grep -q '^nginx\.service'; then
        echo nginx; return
    fi
    if command -v httpd >/dev/null 2>&1 || command -v apache2 >/dev/null 2>&1 \
        || systemctl list-unit-files 2>/dev/null | grep -qE '^(httpd|apache2)\.service'; then
        echo apache; return
    fi
    echo none
}

apache_service_name() {
    command -v httpd >/dev/null 2>&1 && { echo httpd; return; }
    echo apache2
}

# ---------------------------------------------------------------------------
# Config persistence (/etc/noaa-recon-api/install.conf)
# ---------------------------------------------------------------------------
save_config() {
    $SUDO mkdir -p "$(dirname "$CONFIG_FILE")"
    $SUDO tee "$CONFIG_FILE" >/dev/null <<CONF_EOF
INSTALL_DIR="${INSTALL_DIR}"
RUN_USER="${RUN_USER}"
BRANCH="${BRANCH}"
VARIANT="${VARIANT}"
PORT="${PORT}"
NET_MODE="${NET_MODE}"
DOMAIN="${DOMAIN}"
API_PATH="${API_PATH}"
DOMAIN_MODE="${DOMAIN_MODE}"
WEBSERVER="${WEBSERVER}"
HTTPS_ENABLED="${HTTPS_ENABLED}"
AUTH_ENABLED="${AUTH_ENABLED}"
CONF_EOF
    # 644, not 600: this file only holds paths/port/domain — no secrets
    # (the admin password is never persisted here) — and the CLI wrapper's
    # update/uninstall/status subcommands need to read it as a normal user.
    $SUDO chmod 644 "$CONFIG_FILE"
}

load_config() {
    [[ -f "$CONFIG_FILE" ]] || return 1
    # shellcheck disable=SC1090
    if [[ -r "$CONFIG_FILE" ]]; then
        source "$CONFIG_FILE"
    else
        # Falls back to sudo for configs written before the 644 fix above.
        source <($SUDO cat "$CONFIG_FILE")
    fi
}

# ---------------------------------------------------------------------------
# Wizard steps
# ---------------------------------------------------------------------------
choose_version() {
    # Skip the picker if a variant was already chosen (via --variant, or derived
    # from --branch, or restored from an existing install's config).
    if [[ -n "$VARIANT" ]]; then
        log_ok "version: ${VARIANT} (branch ${BRANCH})"
        return
    fi
    menu_select "Which version of the API do you want to install?" \
        "Rust — compiled axum server (experimental, faster). Branch: rust" \
        "Python — the original FastAPI app (stable). Branch: main"
    case "$MENU_INDEX" in
        0) VARIANT="rust";   BRANCH="rust" ;;
        1) VARIANT="python"; BRANCH="main" ;;
    esac
    log_ok "selected: ${VARIANT} (branch ${BRANCH})"
    [[ "$VARIANT" == "rust" ]] && log_warn "Rust variant note: reflectance satellite bands (2/3/5) and composites aren't available yet, and data ingest still runs via Python scripts. See RUST.md."
}

# Rust variant only: install the toolchain (per RUN_USER, minimal profile) and
# compile the release binary. The first build compiles netCDF-C + HDF5 from
# source (a few minutes); CFLAGS downgrades permerrors that GCC >= 14 raises on
# netCDF-C's older source.
# Resolve RUN_USER's cargo binary path (rustup installs with --no-modify-path,
# so cargo isn't on PATH; and $HOME isn't reliable under sudo -u).
_cargo_bin() {
    local uhome; uhome="$(getent passwd "$RUN_USER" | cut -d: -f6)"
    echo "${uhome}/.cargo/bin/cargo"
}

install_rust_toolchain() {
    if [[ -x "$(_cargo_bin)" ]]; then
        log_ok "rust toolchain already present for ${RUN_USER}"
        return
    fi
    log_step "Installing the Rust toolchain (rustup) for ${RUN_USER}"
    run_as "$RUN_USER" bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path' >&2
    [[ -x "$(_cargo_bin)" ]] || die "rustup install did not produce $(_cargo_bin)"
    log_ok "rust toolchain installed"
}

build_rust() {
    local cargo; cargo="$(_cargo_bin)"
    log_step "Building the Rust server (cargo build --release — first build compiles netCDF/HDF5, can take several minutes)"
    run_as "$RUN_USER" bash -c "cd '${INSTALL_DIR}' && CFLAGS='-Wno-error=incompatible-pointer-types -Wno-error=int-conversion -Wno-error=implicit-function-declaration' '${cargo}' build --release -p noaa-recon-api" >&2
    [[ -x "${INSTALL_DIR}/target/release/noaa-recon-api" ]] || die "Rust build did not produce ${INSTALL_DIR}/target/release/noaa-recon-api"
    log_ok "rust binary built -> target/release/noaa-recon-api"
}

choose_install_dir() {
    INSTALL_DIR="$(ask_text "Where should noaa-recon-api live?" "$INSTALL_DIR")"
}

choose_run_user() {
    local default_user="${SUDO_USER:-$(id -un)}"
    [[ "$default_user" == "root" ]] && default_user="noaa-recon-api"
    RUN_USER="$(ask_text "System user to run the API service as" "${RUN_USER:-$default_user}")"
    if ! id "$RUN_USER" >/dev/null 2>&1; then
        if ask_yesno "User '$RUN_USER' doesn't exist yet. Create it now (a dedicated, low-privilege service account)?" y; then
            $SUDO useradd --system --create-home --shell /usr/sbin/nologin "$RUN_USER" 2>/dev/null \
                || $SUDO useradd --system --create-home --shell /sbin/nologin "$RUN_USER"
            log_ok "created user '$RUN_USER'"
        else
            die "Re-run and choose an existing user."
        fi
    fi
}

choose_network_mode() {
    menu_select "How will this API be reached?" \
        "Just this machine (127.0.0.1 only — safest, good for local testing)" \
        "My local network (any device on the LAN, no domain)" \
        "A domain name over the internet (recommended for public use)"
    case "$MENU_INDEX" in
        0) NET_MODE="local" ;;
        1) NET_MODE="lan" ;;
        2) NET_MODE="domain" ;;
    esac

    if [[ "$NET_MODE" == "domain" ]]; then
        DOMAIN="$(ask_text "Domain or subdomain for the API (e.g. api.example.com)" "$DOMAIN")"
        [[ -z "$DOMAIN" ]] && die "A domain is required for this mode."
        menu_select "How should it be reachable at that domain?" \
            "Dedicated subdomain, API at the root (recommended, e.g. https://${DOMAIN}/)" \
            "A path under a site that already exists (e.g. https://${DOMAIN}${API_PATH}/)"
        if [[ "$MENU_INDEX" -eq 0 ]]; then DOMAIN_MODE="subdomain"; API_PATH=""
        else DOMAIN_MODE="path"; API_PATH="$(ask_text "Path prefix" "$API_PATH")"; fi
        PORT="8000"
    else
        PORT="$(ask_text "Port to run the API on" "$PORT")"
    fi
}

choose_webserver() {
    [[ "$NET_MODE" != "domain" ]] && { WEBSERVER="none"; return; }
    local detected; detected="$(detect_webserver)"
    if [[ "$detected" != "none" ]]; then
        log_step "Detected a running $detected"
        if ask_yesno "Reconfigure $detected to reverse-proxy this domain to the API?" y; then
            WEBSERVER="$detected"
        else
            WEBSERVER="none"
            log_warn "Skipping webserver config — you'll need to wire up the reverse proxy yourself. See INSTALL.md."
        fi
        return
    fi
    log_warn "No running webserver detected."
    menu_select "A webserver is needed to put this on a domain with HTTPS. What would you like to do?" \
        "Install and configure nginx (recommended)" \
        "Install and configure Apache" \
        "Skip — run the API directly on its port, no reverse proxy/HTTPS"
    case "$MENU_INDEX" in
        0) WEBSERVER="nginx"; install_webserver_package nginx ;;
        1) WEBSERVER="apache"; install_webserver_package apache ;;
        2) WEBSERVER="none" ;;
    esac
}

clone_or_update_repo() {
    if [[ -d "${INSTALL_DIR}/.git" ]]; then
        log_step "Existing repo at ${INSTALL_DIR} — syncing to origin/${BRANCH}"
        run_as "$RUN_USER" git -C "$INSTALL_DIR" fetch origin >&2
        run_as "$RUN_USER" git -C "$INSTALL_DIR" reset --hard "origin/${BRANCH}" >&2
        run_as "$RUN_USER" git -C "$INSTALL_DIR" submodule update --init --recursive >&2
    else
        log_step "Cloning ${REPO_URL} into ${INSTALL_DIR}"
        $SUDO mkdir -p "$(dirname "$INSTALL_DIR")"
        $SUDO git clone --branch "$BRANCH" --recurse-submodules "$REPO_URL" "$INSTALL_DIR" >&2
        $SUDO chown -R "${RUN_USER}:${RUN_USER}" "$INSTALL_DIR"
    fi
    log_ok "repo ready at ${INSTALL_DIR} ($(run_as "$RUN_USER" git -C "$INSTALL_DIR" rev-parse --short HEAD))"
}

setup_python_env() {
    log_step "Creating the Python virtual environment and installing dependencies (can take a minute)"
    run_as "$RUN_USER" python3 -m venv "${INSTALL_DIR}/.venv"
    run_as "$RUN_USER" "${INSTALL_DIR}/.venv/bin/pip" install --upgrade pip -q
    run_as "$RUN_USER" "${INSTALL_DIR}/.venv/bin/pip" install -e "${INSTALL_DIR}" -q
    log_ok "virtualenv ready"
}

configure_admin_credentials() {
    local cred_file="${INSTALL_DIR}/admin_credentials.json"
    if [[ -f "$cred_file" ]]; then
        log_ok "admin_credentials.json already exists — leaving it alone"
        return
    fi
    log_step "Setting up the admin console login (cache/database management UI)"
    local user pass secret
    user="$(ask_text "Admin console username" "admin")"
    if ask_yesno "Generate a random admin password (recommended)?" y; then
        if [[ "$VARIANT" == "rust" ]]; then
            pass="$(gen_token_urlsafe 16)"
        else
            pass="$(run_as "$RUN_USER" "${INSTALL_DIR}/.venv/bin/python3" -c "import secrets;print(secrets.token_urlsafe(16))")"
        fi
    else
        pass="$(ask_text "Admin console password" "")"
    fi
    if [[ "$VARIANT" == "rust" ]]; then
        secret="$(gen_token_hex 32)"
        run_as "$RUN_USER" bash -c 'cat > "$1" && chmod 600 "$1"' _ "$cred_file" <<EOF
{
  "username": "$(json_escape "$user")",
  "password": "$(json_escape "$pass")",
  "secret_key": "$(json_escape "$secret")"
}
EOF
    else
        secret="$(run_as "$RUN_USER" "${INSTALL_DIR}/.venv/bin/python3" -c "import secrets;print(secrets.token_hex(32))")"
        run_as "$RUN_USER" "${INSTALL_DIR}/.venv/bin/python3" - "$cred_file" "$user" "$pass" "$secret" <<'PYEOF'
import json, os, sys
path, user, pw, secret = sys.argv[1:5]
with open(path, "w") as f:
    json.dump({"username": user, "password": pw, "secret_key": secret}, f, indent=2)
    f.write("\n")
os.chmod(path, 0o600)
PYEOF
    fi
    ADMIN_USER="$user"; ADMIN_PASS="$pass"
    log_ok "admin console credentials set (username: ${user})"
    # This account becomes the first superuser automatically the first time
    # the app starts (app/main.py's startup migration, see
    # app/services/tokens.py's migrate_legacy_admin_credentials) — nothing
    # more to do here beyond collecting the username/password above.
}

configure_auth() {
    local auth_file="${INSTALL_DIR}/auth_config.json"
    if [[ -f "$auth_file" ]]; then
        log_ok "auth_config.json already exists — leaving it alone"
        return
    fi
    log_step "Public API access"
    if ask_yesno "Require an API token for the public data endpoints (satellite/storms/recon/tdr/raw)?" n; then
        AUTH_ENABLED="y"
    else
        AUTH_ENABLED="n"
    fi
    local enabled_json="false"
    [[ "$AUTH_ENABLED" == "y" ]] && enabled_json="true"
    if [[ "$VARIANT" == "rust" ]]; then
        run_as "$RUN_USER" bash -c "cat > \"\$1\"" _ "$auth_file" <<EOF
{
  "enabled": ${enabled_json}
}
EOF
    else
        run_as "$RUN_USER" "${INSTALL_DIR}/.venv/bin/python3" - "$auth_file" "$enabled_json" <<'PYEOF'
import json, sys
path, enabled_json = sys.argv[1:3]
with open(path, "w") as f:
    json.dump({"enabled": json.loads(enabled_json)}, f, indent=2)
    f.write("\n")
PYEOF
    fi
    if [[ "$AUTH_ENABLED" == "y" ]]; then
        log_ok "API tokens required — issue accounts/keys from the admin console's API management pane after setup"
    else
        log_ok "public API left open (default) — you can require tokens later from the admin console"
    fi
}

write_systemd_service() {
    log_step "Writing the systemd service (${VARIANT} variant)"
    local bindhost="127.0.0.1"
    [[ "$NET_MODE" == "lan" ]] && bindhost="0.0.0.0"

    # ExecStart + any extra unit lines differ by variant. The rust binary reads
    # its host/port/repo-root from the environment (config.rs).
    local exec_start extra_lines=""
    if [[ "$VARIANT" == "rust" ]]; then
        exec_start="${INSTALL_DIR}/target/release/noaa-recon-api"
        extra_lines=$'Environment=PORT='"${PORT}"$'\n'"Environment=NOAA_RECON_HOST=${bindhost}"$'\n'"Environment=NOAA_RECON_REPO_ROOT=${INSTALL_DIR}"
        [[ "$NET_MODE" == "domain" && "$DOMAIN_MODE" == "path" ]] && log_warn "Rust variant doesn't support a --root-path prefix; if OpenAPI/doc links look off under ${API_PATH}, that's why (the API itself works)."
    else
        local rootpath_flag=""
        [[ "$NET_MODE" == "domain" && "$DOMAIN_MODE" == "path" ]] && rootpath_flag="--root-path ${API_PATH}"
        exec_start="${INSTALL_DIR}/.venv/bin/uvicorn app.main:app --host ${bindhost} --port ${PORT} ${rootpath_flag}"
    fi

    $SUDO tee "/etc/systemd/system/${SERVICE_NAME}.service" >/dev/null <<SERVICE_EOF
[Unit]
Description=noaa-recon-api (GOES/TDR data API, ${VARIANT})
After=network.target

[Service]
Type=simple
User=${RUN_USER}
WorkingDirectory=${INSTALL_DIR}
${extra_lines}
ExecStart=${exec_start}
Restart=on-failure
RestartSec=5
# Memory guardrail: the API idles around ~100MB, but a pathological render
# (e.g. a Band 2 full-disk request, or several overlapping composites) can
# transiently balloon multi-GB. On a small host that can OOM the whole box;
# capping it here means systemd kills+restarts just this service instead.
# MemoryHigh throttles/reclaims first (soft); MemoryMax is the hard ceiling.
MemoryHigh=2G
MemoryMax=3G

[Install]
WantedBy=multi-user.target
SERVICE_EOF
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable --now "${SERVICE_NAME}.service" >&2
    log_ok "${SERVICE_NAME}.service enabled and started"
}

configure_nginx_subdomain() {
    local conf="/etc/nginx/conf.d/${SERVICE_NAME}.conf"
    $SUDO tee "$conf" >/dev/null <<NGINX_EOF
server {
    listen 80;
    server_name ${DOMAIN};

    location / {
        proxy_pass http://127.0.0.1:${PORT}/;
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_read_timeout 60s;
    }
}
NGINX_EOF
    if $SUDO nginx -t >&2; then
        $SUDO systemctl reload nginx
        log_ok "nginx configured and reloaded for ${DOMAIN}"
    else
        log_err "nginx config test failed — not reloading. Check ${conf}"
    fi
}

configure_nginx_path() {
    $SUDO mkdir -p /etc/nginx/snippets
    local snip="/etc/nginx/snippets/${SERVICE_NAME}.conf"
    $SUDO tee "$snip" >/dev/null <<NGINX_EOF
location ${API_PATH}/ {
    proxy_pass http://127.0.0.1:${PORT}/;
    proxy_http_version 1.1;
    proxy_set_header Host \$host;
    proxy_set_header X-Real-IP \$remote_addr;
    proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto \$scheme;
    proxy_read_timeout 60s;
}
NGINX_EOF
    log_ok "wrote ${snip}"
    local match
    match="$(grep -rls "server_name.*${DOMAIN}" /etc/nginx/conf.d /etc/nginx/sites-enabled 2>/dev/null | head -1 || true)"
    log_warn "Manual step required: inside the existing server {} block for ${DOMAIN}, add:"
    echo "        include ${snip};" >&2
    [[ -n "$match" ]] && echo "      That site's config looks like it's here: ${match}" >&2
    echo "      Then: sudo nginx -t && sudo systemctl reload nginx" >&2
}

configure_apache_subdomain() {
    local svc; svc="$(apache_service_name)"
    local conf
    if [[ "$svc" == "httpd" ]]; then conf="/etc/httpd/conf.d/${SERVICE_NAME}.conf"
    else conf="/etc/apache2/sites-available/${SERVICE_NAME}.conf"; fi
    $SUDO tee "$conf" >/dev/null <<APACHE_EOF
<VirtualHost *:80>
    ServerName ${DOMAIN}
    ProxyPreserveHost On
    ProxyPass / http://127.0.0.1:${PORT}/
    ProxyPassReverse / http://127.0.0.1:${PORT}/
</VirtualHost>
APACHE_EOF
    if [[ "$svc" == "apache2" ]]; then
        $SUDO a2enmod proxy proxy_http >&2 2>/dev/null || true
        $SUDO a2ensite "${SERVICE_NAME}.conf" >&2 2>/dev/null || true
    fi
    if $SUDO apachectl configtest >&2 2>/dev/null || $SUDO httpd -t >&2 2>/dev/null; then
        $SUDO systemctl reload "$svc"
        log_ok "$svc configured and reloaded for ${DOMAIN}"
    else
        log_warn "Couldn't verify the Apache config automatically — check ${conf}, then reload $svc manually."
        $SUDO systemctl reload "$svc" 2>/dev/null || true
    fi
}

configure_apache_path() {
    local svc; svc="$(apache_service_name)"
    local snipdir; snipdir="$([[ "$svc" == "httpd" ]] && echo /etc/httpd/conf.d || echo /etc/apache2/snippets)"
    $SUDO mkdir -p "$snipdir"
    local snip="${snipdir}/${SERVICE_NAME}-snippet.conf"
    $SUDO tee "$snip" >/dev/null <<APACHE_EOF
<Location "${API_PATH}/">
    ProxyPreserveHost On
    ProxyPass "http://127.0.0.1:${PORT}/"
    ProxyPassReverse "http://127.0.0.1:${PORT}/"
</Location>
APACHE_EOF
    log_ok "wrote ${snip}"
    log_warn "Manual step required: inside the existing VirtualHost for ${DOMAIN}, add:"
    echo "        Include ${snip}" >&2
    echo "      Then: sudo systemctl reload $svc" >&2
}

configure_webserver() {
    [[ "$WEBSERVER" == "none" ]] && return
    log_step "Configuring ${WEBSERVER} for ${DOMAIN}${API_PATH}"
    if [[ "$WEBSERVER" == "nginx" ]]; then
        [[ "$DOMAIN_MODE" == "subdomain" ]] && configure_nginx_subdomain || configure_nginx_path
    else
        [[ "$DOMAIN_MODE" == "subdomain" ]] && configure_apache_subdomain || configure_apache_path
    fi
}

configure_https() {
    # Path mode deliberately never touches the existing site's config (see
    # configure_nginx_path/configure_apache_path) — certbot's --nginx/--apache
    # plugins edit the live server block to add the redirect + cert directives,
    # which would break that safety guarantee. Subdomain mode owns its config
    # file outright, so it's safe to hand to certbot there.
    [[ "$WEBSERVER" == "none" || "$DOMAIN_MODE" != "subdomain" ]] && return
    ask_yesno "Set up free HTTPS via Let's Encrypt (certbot)? Requires ${DOMAIN}'s DNS A record already pointing here, and ports 80/443 reachable from the internet." y || return
    case "$PKG_MANAGER" in
        dnf) $SUDO dnf install -y certbot python3-certbot-nginx python3-certbot-apache >&2 ;;
        apt) $SUDO apt-get install -y certbot python3-certbot-nginx python3-certbot-apache >&2 ;;
        nix) nix-env -iA nixpkgs.certbot >&2 ;;
        *)   log_warn "Install certbot manually for this package manager, then run it against ${DOMAIN}."; return ;;
    esac
    local plugin="--nginx"; [[ "$WEBSERVER" == "apache" ]] && plugin="--apache"
    local email; email="$(ask_text "Email for renewal notices (blank to skip)" "")"
    local email_flag="--register-unsafely-without-email"
    [[ -n "$email" ]] && email_flag="-m ${email}"
    if $SUDO certbot $plugin -d "$DOMAIN" --redirect --non-interactive --agree-tos $email_flag >&2; then
        HTTPS_ENABLED="1"
        log_ok "HTTPS enabled for https://${DOMAIN}"
    else
        log_warn "certbot failed — the site is still reachable over plain HTTP. Retry later with: sudo certbot ${plugin} -d ${DOMAIN}"
    fi
}

configure_firewall() {
    local ports=()
    if [[ "$NET_MODE" == "domain" ]]; then ports=(80 443)
    elif [[ "$NET_MODE" == "lan" ]]; then ports=("$PORT")
    else return; fi
    if command -v firewall-cmd >/dev/null 2>&1 && $SUDO systemctl is-active --quiet firewalld 2>/dev/null; then
        local p
        for p in "${ports[@]}"; do
            case "$p" in
                80)  $SUDO firewall-cmd --permanent --add-service=http  >&2 ;;
                443) $SUDO firewall-cmd --permanent --add-service=https >&2 ;;
                *)   $SUDO firewall-cmd --permanent --add-port="${p}/tcp" >&2 ;;
            esac
        done
        $SUDO firewall-cmd --reload >&2
        log_ok "firewalld: opened ${ports[*]}"
    elif command -v ufw >/dev/null 2>&1 && $SUDO ufw status 2>/dev/null | grep -q "Status: active"; then
        local p
        for p in "${ports[@]}"; do $SUDO ufw allow "${p}/tcp" >&2; done
        log_ok "ufw: opened ${ports[*]}"
    fi
}

configure_selinux() {
    [[ "$NET_MODE" != "domain" ]] && return
    command -v getenforce >/dev/null 2>&1 || return
    [[ "$(getenforce)" == "Enforcing" ]] || return
    $SUDO setsebool -P httpd_can_network_connect 1
    log_ok "SELinux: allowed httpd_can_network_connect (needed for the webserver to reach 127.0.0.1:${PORT})"
}

build_archives() {
    log_step "Building the storm-track archive (backs GET /v1/storms/*, usually ~10s)"
    # Storms ingest is native in the rust variant; recon MET is still Python.
    if [[ "$VARIANT" == "rust" ]]; then
        run_as "$RUN_USER" "${INSTALL_DIR}/target/release/noaa-recon-api" ingest-storms >&2
    else
        run_as "$RUN_USER" "${INSTALL_DIR}/.venv/bin/python3" "${INSTALL_DIR}/scripts/ingest_storms.py" >&2
    fi
    log_ok "storm archive built"

    local full_flag=""
    if ask_yesno "Build the FULL recon MET archive now (every hurricane hunter mission since 2011)? This can take SEVERAL HOURS. Choosing no builds just current+previous season (fast, minutes)." n; then
        full_flag="--full"
        log_step "Building the full recon MET archive — this will take a while"
    else
        log_step "Building the recon MET archive (current + previous season)"
    fi
    if [[ "$VARIANT" == "rust" ]]; then
        run_as "$RUN_USER" "${INSTALL_DIR}/target/release/noaa-recon-api" ingest-recon $full_flag >&2
    else
        run_as "$RUN_USER" "${INSTALL_DIR}/.venv/bin/python3" "${INSTALL_DIR}/scripts/ingest_recon_met.py" $full_flag >&2
    fi
    log_ok "recon MET archive built"
}

install_timers() {
    log_step "Installing nightly archive-update and cache-cleanup timers"
    local rust_bin="${INSTALL_DIR}/target/release/noaa-recon-api"
    local py="${INSTALL_DIR}/.venv/bin/python3"
    # TDR ingest is a rust-only subcommand (no scripts/ingest_tdr.py exists —
    # TDR was added after the python variant was frozen), so its timer/service
    # only get installed for the rust variant.
    local svcs=(storm-archive-update recon-met-update goes-nc-cache-cleanup)
    # TDR ingest and the HURDAT2 release check are both rust-only subcommands
    # (no scripts/ingest_tdr.py or scripts/check_hurdat.py exist — they were
    # added after the python variant was frozen), so their timer/service only
    # get installed for the rust variant.
    [[ "$VARIANT" == "rust" ]] && svcs+=(tdr-update hurdat-check)
    local svc
    for svc in "${svcs[@]}"; do
        # Storms + cache cleanup are native subcommands in the rust variant;
        # recon MET is Python in both variants; TDR + hurdat-check are rust-only.
        local desc after="After=network.target" execstart freq="Nightly"
        case "$svc" in
            storm-archive-update)
                desc="storm track archive (HURDAT2 + ATCF) update"
                if [[ "$VARIANT" == "rust" ]]; then execstart="${rust_bin} ingest-storms"; else execstart="${py} scripts/ingest_storms.py"; fi ;;
            recon-met-update)
                desc="recon MET archive update"
                if [[ "$VARIANT" == "rust" ]]; then execstart="${rust_bin} ingest-recon"; else execstart="${py} scripts/ingest_recon_met.py"; fi ;;
            goes-nc-cache-cleanup)
                desc="cleanup of GOES netCDF cache files older than 1 day"; after=""
                if [[ "$VARIANT" == "rust" ]]; then execstart="${rust_bin} clean-nc-cache"; else execstart="${py} scripts/clear_nc_cache.py"; fi ;;
            tdr-update)
                desc="TDR (tail Doppler radar) archive update"; after=""
                execstart="${rust_bin} ingest-tdr" ;;
            hurdat-check)
                desc="check for an updated HURDAT2 best-track release (see issue #11)"; after=""; freq="Weekly"
                execstart="${rust_bin} check-hurdat" ;;
        esac
        $SUDO tee "/etc/systemd/system/${svc}.service" >/dev/null <<TIMER_SVC_EOF
[Unit]
Description=${freq} ${desc}
${after}

[Service]
Type=oneshot
WorkingDirectory=${INSTALL_DIR}
ExecStart=${execstart}
User=${RUN_USER}
Group=${RUN_USER}

[Install]
WantedBy=multi-user.target
TIMER_SVC_EOF
    done
    local timer_files=("${INSTALL_DIR}/deploy/storm-archive-update.timer" "${INSTALL_DIR}/deploy/recon-met-update.timer" "${INSTALL_DIR}/deploy/goes-nc-cache-cleanup.timer")
    local timer_names="storm-archive-update.timer recon-met-update.timer goes-nc-cache-cleanup.timer"
    if [[ "$VARIANT" == "rust" ]]; then
        timer_files+=("${INSTALL_DIR}/deploy/tdr-update.timer" "${INSTALL_DIR}/deploy/hurdat-check.timer")
        timer_names="${timer_names} tdr-update.timer hurdat-check.timer"
    fi
    $SUDO cp "${timer_files[@]}" /etc/systemd/system/
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable --now $timer_names >&2
    log_ok "nightly timers enabled (03:15, 03:45, 04:00 [rust only], and 04:15 server time); hurdat-check runs weekly (Sun 02:00, rust only)"
}

install_cli_wrapper() {
    log_step "Installing the 'noaa-recon-api' command"
    $SUDO tee "$CLI_PATH" >/dev/null <<CLI_EOF
#!/usr/bin/env bash
set -euo pipefail
INSTALL_DIR="${INSTALL_DIR}"
PORT="${PORT}"
case "\${1:-}" in
    update)    exec "\$INSTALL_DIR/install.sh" --update ;;
    uninstall) exec "\$INSTALL_DIR/install.sh" --uninstall ;;
    status)
        systemctl status ${SERVICE_NAME} --no-pager || true
        echo
        curl -fsS "http://127.0.0.1:\$PORT/v1/health" && echo || echo "(health check failed)"
        ;;
    logs)    exec journalctl -u ${SERVICE_NAME} -f ;;
    restart) sudo systemctl restart ${SERVICE_NAME} ;;
    *) echo "Usage: noaa-recon-api {update|status|logs|restart|uninstall}"; exit 1 ;;
esac
CLI_EOF
    $SUDO chmod +x "$CLI_PATH"
    log_ok "try: noaa-recon-api status"
}

print_summary() {
    local url path_suffix=""
    [[ "$NET_MODE" == "domain" && "$DOMAIN_MODE" == "path" ]] && path_suffix="$API_PATH"
    case "$NET_MODE" in
        domain)
            local scheme="http"; [[ -n "$HTTPS_ENABLED" ]] && scheme="https"
            url="${scheme}://${DOMAIN}${path_suffix}"
            ;;
        lan)
            local ip; ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
            [[ -z "$ip" ]] && ip="<this machine LAN IP>"
            url="http://${ip}:${PORT}"
            ;;
        *)
            url="http://127.0.0.1:${PORT}"
            ;;
    esac
    echo >&2
    printf "${c_bold}${c_green}noaa-recon-api is up and running.${c_reset}\n" >&2
    echo >&2
    printf "  API:    %s\n"      "$url" >&2
    printf "  Docs:   %s/docs\n" "$url" >&2
    printf "  Admin:  %s/\n"     "$url" >&2
    if [[ -n "$ADMIN_USER" ]]; then
        printf "  Login:  %s / %s   ${c_dim}(save this — shown once)${c_reset}\n" "$ADMIN_USER" "$ADMIN_PASS" >&2
    fi
    echo >&2
    echo "  Manage it:" >&2
    echo "    noaa-recon-api status     — is it running?" >&2
    echo "    noaa-recon-api logs       — live logs" >&2
    echo "    noaa-recon-api update     — pull the latest from GitHub and restart" >&2
    echo "    noaa-recon-api uninstall  — remove everything" >&2
    echo >&2
    echo "  Config: ${CONFIG_FILE}" >&2
}

# ---------------------------------------------------------------------------
# Top-level commands
# ---------------------------------------------------------------------------
run_wizard() {
    choose_version
    choose_install_dir
    choose_run_user
    choose_network_mode
    choose_webserver
    install_base_packages
    clone_or_update_repo
    # rust variant is fully native (server + ingest); python variant uses a venv.
    if [[ "$VARIANT" == "rust" ]]; then
        install_rust_toolchain
        build_rust
    else
        setup_python_env
    fi
    configure_admin_credentials
    configure_auth
    write_systemd_service
    configure_webserver
    configure_https
    configure_firewall
    configure_selinux
    ask_yesno "Build the storm-track and recon MET archives now?" y && build_archives
    install_timers
    install_cli_wrapper
    save_config
    print_summary
}

cmd_install() {
    print_banner
    PKG_MANAGER="$(detect_pkg_manager)"
    [[ "$PKG_MANAGER" == "unknown" ]] && log_warn "Couldn't detect dnf/apt/nix — you may need to install dependencies yourself."
    log_ok "package manager: ${PKG_MANAGER}"

    if load_config 2>/dev/null; then
        log_step "Existing installation detected at ${INSTALL_DIR}"
        menu_select "What would you like to do?" \
            "Update to the latest version (git pull + restart)" \
            "Reconfigure (re-run the setup wizard)" \
            "Uninstall" \
            "Cancel"
        case "$MENU_INDEX" in
            0) cmd_update; return ;;
            1) : ;;
            2) cmd_uninstall; return ;;
            3) echo "Cancelled." >&2; exit 0 ;;
        esac
    fi
    run_wizard
}

cmd_update() {
    load_config || die "No existing install found at ${CONFIG_FILE}. Run ./install.sh normally first."
    log_step "Updating ${INSTALL_DIR} to the latest ${BRANCH} (${VARIANT:-python})"
    run_as "$RUN_USER" git -C "$INSTALL_DIR" fetch origin >&2
    run_as "$RUN_USER" git -C "$INSTALL_DIR" reset --hard "origin/${BRANCH}" >&2
    run_as "$RUN_USER" git -C "$INSTALL_DIR" submodule update --init --recursive >&2
    if [[ "$VARIANT" == "rust" ]]; then
        build_rust
    else
        run_as "$RUN_USER" "${INSTALL_DIR}/.venv/bin/pip" install -e "${INSTALL_DIR}" -q
    fi
    $SUDO systemctl restart "${SERVICE_NAME}"
    log_ok "updated and restarted — now on $(run_as "$RUN_USER" git -C "$INSTALL_DIR" rev-parse --short HEAD)"
}

cmd_uninstall() {
    load_config || die "Nothing to uninstall — no ${CONFIG_FILE} found."
    log_warn "This stops and removes the noaa-recon-api service, timers, and webserver config."
    ask_yesno "Continue?" n || { echo "Cancelled." >&2; exit 0; }

    $SUDO systemctl disable --now "${SERVICE_NAME}" 2>/dev/null || true
    $SUDO systemctl disable --now storm-archive-update.timer recon-met-update.timer goes-nc-cache-cleanup.timer tdr-update.timer hurdat-check.timer 2>/dev/null || true
    $SUDO rm -f "/etc/systemd/system/${SERVICE_NAME}.service" \
                /etc/systemd/system/storm-archive-update.service /etc/systemd/system/storm-archive-update.timer \
                /etc/systemd/system/recon-met-update.service /etc/systemd/system/recon-met-update.timer \
                /etc/systemd/system/goes-nc-cache-cleanup.service /etc/systemd/system/goes-nc-cache-cleanup.timer \
                /etc/systemd/system/tdr-update.service /etc/systemd/system/tdr-update.timer \
                /etc/systemd/system/hurdat-check.service /etc/systemd/system/hurdat-check.timer
    $SUDO systemctl daemon-reload

    $SUDO rm -f "/etc/nginx/conf.d/${SERVICE_NAME}.conf" "/etc/nginx/snippets/${SERVICE_NAME}.conf"
    $SUDO rm -f "/etc/httpd/conf.d/${SERVICE_NAME}.conf" "/etc/apache2/sites-available/${SERVICE_NAME}.conf" \
                "/etc/apache2/snippets/${SERVICE_NAME}-snippet.conf"
    command -v nginx >/dev/null 2>&1 && { $SUDO nginx -t && $SUDO systemctl reload nginx || true; }

    $SUDO rm -f "$CLI_PATH"

    if ask_yesno "Also delete the installed code and databases at ${INSTALL_DIR}? This deletes data/*.sqlite too and cannot be undone." n; then
        $SUDO rm -rf "${INSTALL_DIR}"
    fi
    $SUDO rm -f "$CONFIG_FILE"
    log_ok "uninstalled"
}

cmd_status() {
    load_config || die "No install found at ${CONFIG_FILE}."
    systemctl status "${SERVICE_NAME}" --no-pager || true
    echo
    curl -fsS "http://127.0.0.1:${PORT}/v1/health" && echo || echo "(health check failed)"
}

# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------
ACTION="install"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --update)    ACTION="update"; shift ;;
        --uninstall) ACTION="uninstall"; shift ;;
        --status)    ACTION="status"; shift ;;
        --dir)       INSTALL_DIR="$2"; shift 2 ;;
        --branch)    BRANCH="$2"; shift 2 ;;
        --variant)   VARIANT="$2"; shift 2 ;;
        -y|--yes)    exec </dev/null; shift ;;
        -h|--help)   print_help; exit 0 ;;
        *) log_err "Unknown option: $1"; print_help; exit 1 ;;
    esac
done

# Reconcile --variant / --branch. An explicit --branch rust implies the rust
# variant; --variant rust implies the rust branch unless a branch was named.
if [[ -n "$VARIANT" && "$VARIANT" != "python" && "$VARIANT" != "rust" ]]; then
    log_err "Unknown --variant: '$VARIANT' (expected 'python' or 'rust')"; exit 1
fi
[[ -z "$VARIANT" && "$BRANCH" == "rust" ]] && VARIANT="rust"
[[ "$VARIANT" == "rust" && "$BRANCH" == "main" ]] && BRANCH="rust"

case "$ACTION" in
    install)   cmd_install ;;
    update)    cmd_update ;;
    uninstall) cmd_uninstall ;;
    status)    cmd_status ;;
esac
