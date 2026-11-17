#!/usr/bin/env bash
# winstats agent installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.sh | bash
#
# Downloads the newest agent from the GitHub releases, installs it, makes it start at boot
# (systemd on Linux, a LaunchAgent on macOS), gives it a token and prints how to add the machine
# in winstats. Running it again updates the agent and keeps its settings and token.
#
# Options (after `bash -s --` when piped):
#   --token TOKEN   use this token (default: a new install gets a random one, an update keeps
#                   the configured one)
#   --name NAME     name shown in winstats (default: the host name)
#   --no-token      no token: anyone on the network may read this machine
#   --uninstall     stop the agent and remove it (its config stays)
# Environment: WINSTATS_TOKEN, WINSTATS_NAME, WINSTATS_VERSION (a tag such as v0.3.4 instead of
# the newest), WINSTATS_BASE (where the files are, for tests: a URL or file:///dir).

set -euo pipefail

REPO="mgvs/winstats"
PORT=47778
BEACON_PORT=47777
TOKEN="${WINSTATS_TOKEN:-}"
NAME="${WINSTATS_NAME:-}"
NO_TOKEN=0
UNINSTALL=0

while [ $# -gt 0 ]; do
    case "$1" in
        --token) TOKEN="${2:?--token needs a value}"; shift 2 ;;
        --name) NAME="${2:?--name needs a value}"; shift 2 ;;
        --no-token) NO_TOKEN=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help) sed -n '2,19p' "$0" 2>/dev/null || true; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

if [ -n "${WINSTATS_BASE:-}" ]; then
    BASE="$WINSTATS_BASE"
elif [ -n "${WINSTATS_VERSION:-}" ]; then
    BASE="https://github.com/$REPO/releases/download/$WINSTATS_VERSION"
else
    BASE="https://github.com/$REPO/releases/latest/download"
fi

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fetch() { # fetch NAME DEST
    say "downloading $1"
    curl -fsSL --retry 3 -o "$2" "$BASE/$1" || die "cannot download $BASE/$1"
}

new_token() {
    if command -v openssl >/dev/null 2>&1; then
        openssl rand -hex 16
    else
        head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n'
    fi
}

# set_key FILE KEY VALUE: `key = "value"` in the agent.toml (replaced, or added at the end)
set_key() {
    local file="$1" key="$2" value="$3" esc
    esc="$(printf '%s' "$value" | sed 's/[\\"]/\\&/g')"
    if grep -q "^$key = " "$file"; then
        # a temp file and cat back: keeps the owner and mode of the config
        awk -v k="$key" -v v="$esc" 'index($0, k " = ") == 1 { print k " = \"" v "\""; next } { print }' "$file" > "$TMP/cfg"
        $SUDO sh -c "cat '$TMP/cfg' > '$file'"
    else
        printf '%s = "%s"\n' "$key" "$esc" | $SUDO tee -a "$file" >/dev/null
    fi
}

get_key() { # get_key FILE KEY: the string value, empty when missing
    $SUDO sed -n "s/^$2 = \"\\(.*\\)\"\$/\\1/p" "$1" 2>/dev/null | head -n1
}

# --- token and name into the config, shared by Linux and macOS --------------------------------
configure() { # configure BIN CONFIG
    local bin="$1" conf="$2" created=0
    if ! $SUDO test -f "$conf"; then
        created=1
        # the agent writes its config with a fresh id on the first run
        $SUDO "$bin" --config "$conf" --dump >/dev/null 2>&1 || true
    fi
    $SUDO test -f "$conf" || die "the agent did not create $conf"
    # the token is in there: readable by its owner only
    $SUDO chmod 600 "$conf"
    local current
    current="$(get_key "$conf" token)"
    # a new install gets a token; an update keeps what is configured (even none) unless told
    if [ "$NO_TOKEN" = 1 ]; then
        TOKEN=""
    elif [ -z "$TOKEN" ]; then
        if [ "$created" = 1 ]; then TOKEN="$(new_token)"; else TOKEN="$current"; fi
    fi
    if [ "$TOKEN" != "$current" ]; then
        set_key "$conf" token "$TOKEN"
    fi
    if [ -n "$NAME" ]; then
        set_key "$conf" name "$NAME"
    fi
}

summary() { # summary ADDRESSES
    echo
    say "winstats agent is running on port $PORT"
    echo
    echo "  In winstats (right-click the taskbar strip) > Remote machines:"
    echo "    on the same network the machine shows up by itself: open it and pick Connect;"
    echo "    elsewhere (another subnet, a VPN, a cloud VM): Add by address..."
    echo
    for a in $1; do echo "    address  $a"; done
    if [ -n "$TOKEN" ]; then
        echo "    token    $TOKEN"
    else
        echo "    token    (none: anyone on the network can read this machine)"
    fi
    echo
    echo "  Over the internet keep the port closed and use an SSH tunnel or a VPN instead."
}

# ================================================================== Linux
linux() {
    local arch
    case "$(uname -m)" in
        x86_64|amd64) arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *) die "no agent build for $(uname -m) yet (there are x86_64 and aarch64)" ;;
    esac

    SUDO=""
    local system=1
    if [ "$(id -u)" != 0 ]; then
        if command -v sudo >/dev/null 2>&1; then
            SUDO="sudo"
            if ! sudo -n true 2>/dev/null; then
                say "system install: sudo asks for your password"
                sudo -v || die "sudo failed"
            fi
        else
            system=0
            warn "no root and no sudo: installing for $(id -un) only"
        fi
    fi
    local has_systemd=0
    [ -d /run/systemd/system ] && has_systemd=1

    local bin unit conf
    if [ "$system" = 1 ]; then
        bin=/usr/local/bin/winstats-agent
        unit=/etc/systemd/system/winstats-agent.service
        conf=/etc/winstats-agent/agent.toml
    else
        bin="$HOME/.local/bin/winstats-agent"
        unit="$HOME/.config/systemd/user/winstats-agent.service"
        conf="${XDG_CONFIG_HOME:-$HOME/.config}/winstats-agent/agent.toml"
    fi
    # an earlier install keeps its config wherever its unit points
    if [ -f "$unit" ]; then
        local was
        was="$(sed -n 's/^ExecStart=.*--config \([^ ]*\).*/\1/p' "$unit" | head -n1)"
        [ -n "$was" ] && conf="$was"
    fi

    if [ "$UNINSTALL" = 1 ]; then
        if [ -x "$bin" ]; then
            if [ "$system" = 1 ]; then $SUDO "$bin" uninstall-service; else "$bin" uninstall-service; fi
        fi
        $SUDO rm -f "$bin"
        say "removed; the config stays in $conf"
        exit 0
    fi

    fetch "winstats-agent-linux-$arch" "$TMP/winstats-agent"
    chmod 755 "$TMP/winstats-agent"
    "$TMP/winstats-agent" --help >/dev/null 2>&1 || [ $? -le 2 ] || die "the downloaded agent does not run here"

    # into place by rename: a running agent keeps its old file until it restarts
    $SUDO mkdir -p "$(dirname "$bin")" "$(dirname "$conf")"
    $SUDO cp "$TMP/winstats-agent" "$bin.new"
    $SUDO chmod 755 "$bin.new"
    $SUDO mv -f "$bin.new" "$bin"
    say "installed $bin (linux $arch)"

    configure "$bin" "$conf"

    if [ "$has_systemd" = 1 ]; then
        if [ "$system" = 1 ]; then
            $SUDO "$bin" --config "$conf" install-service >/dev/null
            $SUDO systemctl restart winstats-agent
            sleep 1
            $SUDO systemctl is-active --quiet winstats-agent || { $SUDO systemctl status winstats-agent --no-pager || true; die "the service did not start"; }
            say "systemd service winstats-agent enabled and running"
        else
            "$bin" --config "$conf" install-service >/dev/null
            systemctl --user restart winstats-agent
            say "systemd user service winstats-agent running"
        fi
    else
        warn "no systemd here: starting the agent in the background now; add this to your init system to start it at boot:"
        echo "    $bin --config $conf"
        $SUDO pkill -x winstats-agent 2>/dev/null || true
        $SUDO sh -c "nohup '$bin' --config '$conf' >/var/log/winstats-agent.log 2>&1 &" 2>/dev/null \
            || nohup "$bin" --config "$conf" >/dev/null 2>&1 &
    fi

    # an active host firewall drops masters and beacons; it is not opened here: on a cloud VM
    # that would put the agent on the internet
    if command -v ufw >/dev/null 2>&1 && $SUDO ufw status 2>/dev/null | grep -q "Status: active"; then
        warn "ufw is active; to let winstats in from your network (adjust the subnet):"
        echo "    sudo ufw allow from 192.168.0.0/16 to any port $PORT proto tcp"
        echo "    sudo ufw allow from 192.168.0.0/16 to any port $BEACON_PORT proto udp"
    elif command -v firewall-cmd >/dev/null 2>&1 && $SUDO firewall-cmd --state >/dev/null 2>&1; then
        warn "firewalld is active; to let winstats in from a trusted network:"
        echo "    sudo firewall-cmd --permanent --zone=trusted --add-source=192.168.0.0/16"
        echo "    sudo firewall-cmd --permanent --add-port=$PORT/tcp --add-port=$BEACON_PORT/udp --zone=trusted && sudo firewall-cmd --reload"
    fi

    local addrs
    # the machine's own addresses, not the container bridges
    addrs="$(ip -4 -o addr show scope global 2>/dev/null | awk '$2 !~ /^(docker|br-|veth|virbr|cni|flannel|lxc)/ { split($4, a, "/"); print a[1] }' | head -n 4 | tr '\n' ' ')"
    [ -n "$addrs" ] || addrs="$(hostname -I 2>/dev/null | tr ' ' '\n' | grep -v ':' | grep -v '^$' | head -n 4 | tr '\n' ' ')"
    summary "${addrs:-$(hostname)}"
}

# ================================================================== macOS
macos() {
    SUDO=""
    [ "$(id -u)" = 0 ] && die "run it as your user, without sudo: the agent is a LaunchAgent of the logged-in user"
    local apps=/Applications
    [ -w "$apps" ] || { apps="$HOME/Applications"; mkdir -p "$apps"; }
    local plist="$HOME/Library/LaunchAgents/org.mgvs.winstats-agent.plist"
    # an earlier install may live in the other Applications folder
    local app="$apps/winstats-agent.app"
    for a in /Applications/winstats-agent.app "$HOME/Applications/winstats-agent.app"; do
        [ -d "$a" ] && app="$a" && apps="$(dirname "$a")" && break
    done
    local bin="$app/Contents/MacOS/winstats-agent"
    local conf="${XDG_CONFIG_HOME:-$HOME/.config}/winstats-agent/agent.toml"
    if [ -f "$plist" ]; then
        local was
        was="$(plutil -extract ProgramArguments.2 raw "$plist" 2>/dev/null || true)"
        [ -n "$was" ] && conf="$was"
    fi

    if [ "$UNINSTALL" = 1 ]; then
        [ -x "$bin" ] && "$bin" uninstall-service
        rm -rf "$app"
        say "removed; the config stays in $conf"
        exit 0
    fi

    fetch winstats-agent-macos.zip "$TMP/agent.zip"
    ditto -x -k "$TMP/agent.zip" "$TMP/x" || die "cannot unpack the agent"
    [ -x "$TMP/x/winstats-agent.app/Contents/MacOS/winstats-agent" ] || die "the archive has no winstats-agent.app"
    xattr -dr com.apple.quarantine "$TMP/x/winstats-agent.app" 2>/dev/null || true

    # stop the old one before its app is replaced (KeepAlive would restart it from a half copy)
    [ -f "$plist" ] && launchctl unload "$plist" 2>/dev/null || true
    rm -rf "$app.old"
    [ -d "$app" ] && mv "$app" "$app.old"
    ditto "$TMP/x/winstats-agent.app" "$app"
    rm -rf "$app.old"
    say "installed $app"

    mkdir -p "$(dirname "$conf")"
    configure "$bin" "$conf"
    # its "Unload failed" for the agent stopped above is no news: the check below tells
    "$bin" --config "$conf" install-service >/dev/null 2>&1 || true
    sleep 1
    launchctl list org.mgvs.winstats-agent >/dev/null 2>&1 || die "the LaunchAgent did not start; see ~/Library/Logs/winstats-agent.log"
    say "LaunchAgent org.mgvs.winstats-agent running (starts at login)"

    local addrs="" a
    for i in en0 en1 en2 en3; do
        a="$(ipconfig getifaddr "$i" 2>/dev/null || true)"
        [ -n "$a" ] && addrs="$addrs $a"
    done
    summary "${addrs:-$(hostname)}"
}

command -v curl >/dev/null 2>&1 || die "curl is needed"
case "$(uname -s)" in
    Linux) linux ;;
    Darwin) macos ;;
    *) die "this script is for Linux and macOS; on Windows use install/agent.ps1" ;;
esac
