#!/usr/bin/env bash
# forgenerd installer: one static binary + systemd unit on a Linux VPS.
#
#   curl -fsSL https://raw.githubusercontent.com/forgenerd/forgenerd/main/scripts/install.sh | sudo bash
#
# Re-running upgrades the binary in place (data and secrets are kept).
#
# Options:
#   --version vX.Y.Z    release to install (default: latest)
#   --repo owner/name   GitHub repository to download from (default: forgenerd/forgenerd)
#   --binary PATH       install a local binary instead of downloading
#   --public-panel      open the panel on http://<server-ip>:4367 (no SSH tunnel needed)
#   --private-panel     panel on 127.0.0.1:4367 only (the default)
#   --no-swap           do not create a swapfile on small machines
#   --dry-run           print what would happen; change nothing
set -euo pipefail

REPO="${FORGENERD_REPO:-forgenerd/forgenerd}"
VERSION="latest"
LOCAL_BINARY=""
CREATE_SWAP=1
PUBLIC_PANEL=""
DRY_RUN=0

DATA_DIR="/var/lib/forgenerd"
BIN_PATH="/usr/local/bin/forgenerd"
UNIT_PATH="/etc/systemd/system/forgenerd.service"
SERVICE_USER="forgenerd"
PANEL_PORT=4367

usage() {
	cat <<'USAGE'
forgenerd installer. Re-running upgrades the binary in place (data and secrets are kept).

  --version vX.Y.Z    release to install (default: latest)
  --repo owner/name   GitHub repository to download from (default: forgenerd/forgenerd)
  --binary PATH       install a local binary instead of downloading
  --public-panel      open the panel on http://<server-ip>:4367 (no SSH tunnel needed)
  --private-panel     panel on 127.0.0.1:4367 only (the default; kept across upgrades)
  --no-swap           do not create a swapfile on small machines
  --dry-run           print what would happen; change nothing
USAGE
}
bold() { printf '\033[1m%s\033[0m\n' "$*"; }
info() { printf '  %s\n' "$*"; }
warn() { printf '\033[33m! %s\033[0m\n' "$*" >&2; }
die() { printf '\033[31mx %s\033[0m\n' "$*" >&2; exit 1; }
run() {
	if [ "$DRY_RUN" -eq 1 ]; then printf '  [dry-run] %s\n' "$*"; else "$@"; fi
}

while [ $# -gt 0 ]; do
	case "$1" in
		--version) VERSION="${2:?--version needs a value}"; shift 2 ;;
		--repo) REPO="${2:?--repo needs a value}"; shift 2 ;;
		--binary) LOCAL_BINARY="${2:?--binary needs a path}"; shift 2 ;;
		--public-panel) PUBLIC_PANEL=1; shift ;;
		--private-panel) PUBLIC_PANEL=0; shift ;;
		--no-swap) CREATE_SWAP=0; shift ;;
		--dry-run) DRY_RUN=1; shift ;;
		-h|--help) usage; exit 0 ;;
		*) die "unknown option: $1 (see --help)" ;;
	esac
done

# ---------------------------------------------------------------- preflight
bold "forgenerd installer"

[ "$(uname -s)" = "Linux" ] || die "forgenerd runs on Linux only."
case "$(uname -m)" in
	x86_64|amd64) TARGET="x86_64-unknown-linux-musl" ;;
	aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
	*) die "unsupported CPU architecture: $(uname -m)" ;;
esac
if [ "$DRY_RUN" -eq 0 ] && [ "$(id -u)" -ne 0 ]; then
	die "run as root (e.g. with sudo)."
fi
command -v systemctl >/dev/null || die "systemd is required."

UPGRADE=0
[ -x "$BIN_PATH" ] && UPGRADE=1
# Without a flag, an upgrade keeps the current panel exposure.
if [ -z "$PUBLIC_PANEL" ]; then
	PUBLIC_PANEL=0
	if grep -q '^Environment=FORGENERD_LISTEN=0.0.0.0:' "$UNIT_PATH" 2>/dev/null; then PUBLIC_PANEL=1; fi
fi
LISTEN_LINE=""
if [ "$PUBLIC_PANEL" -eq 1 ]; then LISTEN_LINE="Environment=FORGENERD_LISTEN=0.0.0.0:$PANEL_PORT"; fi
# The panel's update check compares the running version with this repository's releases.
REPO_LINE=""
if [ "$REPO" != "forgenerd/forgenerd" ]; then REPO_LINE="Environment=FORGENERD_REPO=$REPO"; fi

bold "Checking Docker"
command -v docker >/dev/null || die "Docker Engine is not installed. Install it first: https://docs.docker.com/engine/install/"
docker info >/dev/null 2>&1 || die "Docker is installed but not running (or not accessible). Start it: systemctl enable --now docker"
docker compose version >/dev/null 2>&1 || die "The Docker Compose plugin is missing. Install the docker-compose-plugin package."
info "$(docker version --format 'Docker {{.Server.Version}}') · $(docker compose version --short 2>/dev/null | sed 's/^/Compose /')"
docker buildx version >/dev/null 2>&1 || warn "docker buildx not found: git builds will use the legacy builder."
command -v git >/dev/null || warn "git is not installed: git-based apps cannot build until it is (apt install git)."

port_in_use() {
	if command -v ss >/dev/null; then ss -Hltn "sport = :$1" | grep -q .; else return 1; fi
}
if [ "$UPGRADE" -eq 0 ]; then
	bold "Checking ports"
	for p in 80 443 "$PANEL_PORT"; do
		if port_in_use "$p"; then
			die "port $p is already in use. forgenerd needs 80/443 for its proxy and $PANEL_PORT for the panel."
		fi
	done
	info "80, 443 and $PANEL_PORT are free"
fi

# ---------------------------------------------------------------- swap
mem_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo)
swap_kb=$(awk '/SwapTotal/ {print $2}' /proc/meminfo)
if [ "$CREATE_SWAP" -eq 1 ] && [ "$swap_kb" -eq 0 ] && [ "$mem_kb" -lt 2097152 ] && [ ! -e /swapfile ]; then
	bold "Creating a 2 GB swapfile (this machine has $((mem_kb / 1024)) MB RAM and no swap)"
	info "Image builds need more memory than a 1 GB VPS has. Skip with --no-swap."
	run fallocate -l 2G /swapfile
	run chmod 600 /swapfile
	run mkswap /swapfile
	run swapon /swapfile
	if ! grep -q '^/swapfile ' /etc/fstab; then
		if [ "$DRY_RUN" -eq 1 ]; then info "[dry-run] append /swapfile to /etc/fstab"; else echo '/swapfile none swap sw 0 0' >> /etc/fstab; fi
	fi
fi

# ---------------------------------------------------------------- binary
bold "Installing the forgenerd binary"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -n "$LOCAL_BINARY" ]; then
	[ -f "$LOCAL_BINARY" ] || die "no such file: $LOCAL_BINARY"
	cp "$LOCAL_BINARY" "$tmp/forgenerd"
	info "using local binary $LOCAL_BINARY"
else
	command -v curl >/dev/null || die "curl is required to download the release."
	if [ "$VERSION" = "latest" ]; then
		base="https://github.com/$REPO/releases/latest/download"
	else
		base="https://github.com/$REPO/releases/download/$VERSION"
	fi
	asset="forgenerd-$TARGET"
	info "downloading $asset from $REPO ($VERSION)"
	run curl -fsSL --retry 3 -o "$tmp/forgenerd" "$base/$asset"
	run curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
	if [ "$DRY_RUN" -eq 0 ]; then
		expected=$(awk -v a="$asset" '$2 == a || $2 == "*"a {print $1}' "$tmp/SHA256SUMS")
		[ -n "$expected" ] || die "SHA256SUMS has no entry for $asset."
		actual=$(sha256sum "$tmp/forgenerd" | awk '{print $1}')
		[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual)."
		info "checksum verified"
	fi
fi
if [ "$DRY_RUN" -eq 0 ]; then
	chmod 755 "$tmp/forgenerd"
	"$tmp/forgenerd" --version >/dev/null 2>&1 || die "the downloaded binary does not run on this machine."
fi
[ "$UPGRADE" -eq 1 ] && run systemctl stop forgenerd
run install -m 0755 "$tmp/forgenerd" "$BIN_PATH"
[ "$DRY_RUN" -eq 1 ] || info "installed $BIN_PATH"

# ---------------------------------------------------------------- user and data
if ! id "$SERVICE_USER" >/dev/null 2>&1; then
	bold "Creating the $SERVICE_USER system user"
	run useradd --system --home-dir "$DATA_DIR" --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
fi
# Docker socket access is root-equivalent; it is what forgenerd exists to use.
run usermod -aG docker "$SERVICE_USER"
run install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$DATA_DIR"

# ---------------------------------------------------------------- systemd
bold "Writing the systemd service"
unit=$(cat <<EOF
[Unit]
Description=forgenerd self-hosted PaaS
Documentation=https://github.com/${REPO:-forgenerd/forgenerd}
After=network-online.target docker.service
Wants=network-online.target
Requires=docker.service

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_USER
SupplementaryGroups=docker
Environment=FORGENERD_DATA_DIR=$DATA_DIR
Environment=HOME=$DATA_DIR
$LISTEN_LINE
$REPO_LINE
ExecStart=$BIN_PATH server
Restart=on-failure
RestartSec=3
# Hardening: forgenerd only writes its data directory; Docker does the rest.
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=$DATA_DIR
ProtectHome=yes
PrivateTmp=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
EOF
)
if [ "$DRY_RUN" -eq 1 ]; then info "[dry-run] write $UNIT_PATH"; else printf '%s\n' "$unit" > "$UNIT_PATH"; fi
run systemctl daemon-reload
run systemctl enable --now forgenerd
if [ "$UPGRADE" -eq 1 ]; then run systemctl restart forgenerd; fi

# ---------------------------------------------------------------- firewall
# Caddy (in Docker) reaches the panel on the forgenerd-internal gateway address. UFW's
# default INPUT policy drops that traffic, which would break the panel domain.
if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q 'Status: active'; then
	bold "Configuring UFW"
	run ufw allow 80/tcp
	run ufw allow 443/tcp
	run ufw allow 443/udp
	if [ "$PUBLIC_PANEL" -eq 1 ]; then run ufw allow "$PANEL_PORT"/tcp comment 'forgenerd: public panel'; fi
	subnet=""
	for _ in $(seq 20); do
		subnet=$(docker network inspect forgenerd-internal --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}' 2>/dev/null || true)
		[ -n "$subnet" ] && break
		[ "$DRY_RUN" -eq 1 ] && break
		sleep 1
	done
	if [ -n "$subnet" ]; then
		run ufw allow in from "$subnet" to any port "$PANEL_PORT" proto tcp comment 'forgenerd: Caddy to panel'
	else
		warn "forgenerd-internal not found yet; after it starts, run: ufw allow in from <its subnet> to any port $PANEL_PORT proto tcp"
	fi
fi

# ---------------------------------------------------------------- done
access() {
	if [ "$PUBLIC_PANEL" -eq 1 ]; then
		cat <<EOF
  Open the panel in your browser:  http://<this-server's-public-IP>:$PANEL_PORT
    Allow inbound TCP $PANEL_PORT in your cloud provider's firewall (e.g. Lightsail
    Networking tab, EC2 security group). This is plain HTTP: once the panel domain
    serves HTTPS, re-run this installer with --private-panel and close the port.
EOF
	else
		cat <<EOF
  Open the panel, either way:
    A. Browser: re-run this installer with --public-panel, allow inbound TCP $PANEL_PORT
       in your cloud firewall, then visit http://<this-server's-public-IP>:$PANEL_PORT
    B. SSH tunnel (nothing exposed): ssh -L $PANEL_PORT:127.0.0.1:$PANEL_PORT <user>@<this-server>
       then visit http://localhost:$PANEL_PORT
EOF
	fi
}

echo
if [ "$DRY_RUN" -eq 1 ]; then bold "Dry run complete: nothing was changed."; echo; access; exit 0; fi
if [ "$UPGRADE" -eq 1 ]; then
	bold "forgenerd upgraded and restarted."
	echo
	access
	exit 0
fi
bold "forgenerd is running."
token=""
for _ in $(seq 20); do
	token=$(journalctl -u forgenerd --no-pager -n 100 2>/dev/null | grep -A3 'setup token' | grep -oE '[A-Za-z0-9_-]{40,}' | tail -1 || true)
	[ -n "$token" ] && break
	sleep 1
done
echo
access
cat <<EOF

  Create the admin account with this one-time setup token:
    ${token:-"(see: journalctl -u forgenerd | grep -A3 'setup token')"}
  Then, in Settings, set the root domain; the panel moves to https://hero.<root-domain>.

  IMPORTANT: back up $DATA_DIR/master.key somewhere safe and separate from the server.
  Every stored secret (env vars, S3 keys, tokens) is encrypted with it; without it they
  cannot be recovered.

  Logs:    journalctl -u forgenerd -f
  Upgrade: re-run this installer.
EOF
