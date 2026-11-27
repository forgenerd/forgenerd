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
#   --public-panel      open the panel on http://<server-ip>:4367 (default for new installs)
#   --private-panel     panel on 127.0.0.1:4367 only
#   --no-swap           do not create a swapfile on small machines
#   --dry-run           preview the install without changing the host
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
UPDATER_PATH="/usr/local/libexec/forgenerd-update"
UPDATER_UNIT="/etc/systemd/system/forgenerd-update.service"
UPDATER_WATCH="/etc/systemd/system/forgenerd-update.path"
SERVICE_USER="forgenerd"
PANEL_PORT=4367
TOTAL_STEPS=7

# ---------------------------------------------------------------- terminal UI
# Colors only on an interactive terminal (and never with NO_COLOR);
# Unicode glyphs only when the locale is UTF-8. Piped output stays plain, readable text.
UI_TTY=0
if [ -t 1 ] && [ "${TERM:-dumb}" != "dumb" ]; then UI_TTY=1; fi
COLOR=0
if [ "$UI_TTY" -eq 1 ] && [ -z "${NO_COLOR:-}" ]; then COLOR=1; fi
UTF8=0
case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in *[Uu][Tt][Ff]-8*|*[Uu][Tt][Ff]8*) UTF8=1 ;; esac

if [ "$COLOR" -eq 1 ]; then
	# The logo's warm amber (#ffb20c) is the accent.
	ACCENT=$'\033[38;2;255;178;12m'
	ACCENT_TEXT=$'\033[38;2;255;211;74m'
	BOLD=$'\033[1m'
	DIM=$'\033[2m'
	GREEN=$'\033[38;2;117;219;160m'
	YELLOW=$'\033[38;2;255;187;85m'
	RED=$'\033[38;2;255;159;153m'
	RESET=$'\033[0m'
else
	ACCENT="" ACCENT_TEXT="" BOLD="" DIM="" GREEN="" YELLOW="" RED="" RESET=""
fi

if [ "$UTF8" -eq 1 ]; then
	G_TEE="├" G_END="└" G_OK="✓" G_FAIL="✕" G_DOT="·" G_ARROW="→"
	B_TL="╭" B_TR="╮" B_BL="╰" B_BR="╯" B_H="─" B_V="│"
else
	G_TEE="|-" G_END="\`-" G_OK="+" G_FAIL="x" G_DOT="-" G_ARROW="->"
	B_TL="+" B_TR="+" B_BL="+" B_BR="+" B_H="-" B_V="|"
fi

INDENT="        "   # aligns details under a step title ("  [1/7] ")
STEP=0
STEP_TITLE=""
STEP_ACTIVE=0
STEP_SUMMARY=""
STEP_STARTED=0

now_ms() {
	if [ -n "${EPOCHREALTIME:-}" ]; then
		local t=${EPOCHREALTIME/[.,]/}
		echo $((10#$t / 1000))
	else
		echo $(($(date +%s) * 1000))
	fi
}
fmt_ms() {
	local ms=$1
	if [ "$ms" -lt 60000 ]; then
		printf '%d.%ds' $((ms / 1000)) $((ms % 1000 / 100))
	else
		printf '%dm%02ds' $((ms / 60000)) $((ms % 60000 / 1000))
	fi
}
STARTED_AT=$(now_ms)

# Close the current step: "└ ✓ summary" with the step's elapsed time right-aligned.
step_done() {
	[ "$STEP_ACTIVE" -eq 1 ] || return 0
	local text="${STEP_SUMMARY:-done}" took pad
	took=$(fmt_ms $(($(now_ms) - STEP_STARTED)))
	pad=$((56 - ${#text}))
	[ "$pad" -ge 2 ] || pad=2
	printf '%s%s%s%s %s%s%s %s%*s%s%s%s\n' "$INDENT" "$DIM" "$G_END" "$RESET" "$GREEN" "$G_OK" "$RESET" "$text" "$pad" "" "$DIM" "$took" "$RESET"
	STEP_ACTIVE=0
	STEP_SUMMARY=""
}
# Start a step: "[n/7] Title".
step() {
	step_done
	STEP=$((STEP + 1))
	STEP_TITLE="$*"
	STEP_ACTIVE=1
	STEP_STARTED=$(now_ms)
	printf '  %s%s[%d/%d]%s %s%s%s\n' "$ACCENT" "$BOLD" "$STEP" "$TOTAL_STEPS" "$RESET" "$BOLD" "$STEP_TITLE" "$RESET"
}
# A detail line nested under the current step.
detail() {
	printf '%s%s%s%s %s\n' "$INDENT" "$DIM" "$G_TEE" "$RESET" "$*"
}
# What the current step achieved, shown on its closing line.
result() { STEP_SUMMARY="$*"; }
warn() {
	printf '%s%s%s%s %s!%s %s\n' "$INDENT" "$DIM" "$G_TEE" "$RESET" "$YELLOW" "$RESET" "$*" >&2
}
die() {
	if [ "$STEP_ACTIVE" -eq 1 ]; then
		printf '%s%s%s%s %s%s %s%s\n' "$INDENT" "$DIM" "$G_END" "$RESET" "$RED" "$G_FAIL" "$*" "$RESET" >&2
	else
		printf '  %s%s %s%s\n' "$RED" "$G_FAIL" "$*" "$RESET" >&2
	fi
	exit 1
}
run() {
	if [ "$DRY_RUN" -eq 1 ]; then
		detail "$DIM\$ $*$RESET"
	else
		"$@"
	fi
}

# Pixel art with half blocks: each text row shows two pixel rows. Rows are strings of
# palette keys ('.' is transparent); PALETTE maps a key to "r;g;b". Fills PIXEL_LINES.
declare -A PALETTE=()
PIXEL_LINES=()
render_pixels() {
	local rows=("$@") r c top bot line
	PIXEL_LINES=()
	for ((r = 0; r < ${#rows[@]}; r += 2)); do
		line=""
		for ((c = 0; c < ${#rows[r]}; c++)); do
			top=${rows[r]:c:1}
			bot=${rows[r + 1]:c:1}
			if [ "$top" = "." ] && [ "$bot" = "." ]; then
				line+=" "
			elif [ "$COLOR" -eq 0 ]; then
				if [ "$top" = "." ]; then line+="▄"; elif [ "$bot" = "." ]; then line+="▀"; else line+="█"; fi
			elif [ "$top" = "." ]; then
				line+=$'\033[38;2;'"${PALETTE[$bot]}m▄"$'\033[0m'
			elif [ "$bot" = "." ] ; then
				line+=$'\033[38;2;'"${PALETTE[$top]}m▀"$'\033[0m'
			elif [ "$top" = "$bot" ]; then
				line+=$'\033[38;2;'"${PALETTE[$top]}m█"$'\033[0m'
			else
				line+=$'\033[38;2;'"${PALETTE[$top]};48;2;${PALETTE[$bot]}m▀"$'\033[0m'
			fi
		done
		PIXEL_LINES+=("$line")
	done
}

system_line() {
	local os arch mem docker_v
	# shellcheck source=/dev/null
	os=$(. /etc/os-release 2>/dev/null && printf '%s' "${PRETTY_NAME:-}") || true
	arch=$(uname -m)
	mem=$(awk '/MemTotal/ {printf "%.1f GB RAM", $2 / 1048576}' /proc/meminfo 2>/dev/null || true)
	docker_v="Docker not found"
	if command -v docker >/dev/null 2>&1; then
		docker_v=$(docker --version 2>/dev/null | sed -n 's/^Docker version \([^,]*\).*/Docker \1/p')
		[ -n "$docker_v" ] || docker_v="Docker"
	fi
	printf '%s' "${os:-$(uname -s)} $G_DOT $arch${mem:+ $G_DOT $mem} $G_DOT $docker_v"
}

banner() {
	local release="installer $G_DOT ${VERSION/#latest/latest release}"
	local sys
	sys=$(system_line)
	echo
	if [ "$UTF8" -eq 0 ]; then
		printf '  %s%sforgenerd%s %s\n  %s%s%s\n' "$ACCENT_TEXT" "$BOLD" "$RESET" "$release" "$DIM" "$sys" "$RESET"
	else
		# Two-row block wordmark.
		local word=("█▀▀ █▀█ █▀█ █▀▀ █▀▀ █▄ █ █▀▀ █▀█ █▀▄" "█▀  █▄█ █▀▄ █▄█ ██▄ █ ▀█ ██▄ █▀▄ █▄▀")
		local cols
		cols=$(tput cols 2>/dev/null || echo 80)
		if [ "$COLOR" -eq 1 ] && [ "$cols" -ge 80 ]; then
			# The forgenerd mascot: an anvil with horns, glasses and a base.
			PALETTE[L]="255;214;48" PALETTE[Y]="255;178;12" PALETTE[D]="217;138;0"
			PALETTE[K]="26;26;24" PALETTE[W]="241;241;237"
			render_pixels \
				"LLLLLLLLLLLLLLLLLLLL" \
				".DYYYYYYYYYYYYYYYYD." \
				"...DYYYYYYYYYYYYD..." \
				".....YYYYYYYYYY....." \
				".....KKKKKKKKKK....." \
				".....KWWKYYKWWK....." \
				".....KWWKYYKWWK....." \
				".....DKKKYYKKKD....." \
				"......DYYYYYYD......" \
				"...DYYYYYYYYYYYYD..."
			local art=("${PIXEL_LINES[@]}")
			printf '  %s   %s%s%s\n' "${art[0]}" "$ACCENT_TEXT$BOLD" "${word[0]}" "$RESET"
			printf '  %s   %s%s%s\n' "${art[1]}" "$ACCENT_TEXT$BOLD" "${word[1]}" "$RESET"
			printf '  %s\n' "${art[2]}"
			printf '  %s   %s%s%s\n' "${art[3]}" "$ACCENT_TEXT" "$release" "$RESET"
			printf '  %s   %s%s%s\n' "${art[4]}" "$DIM" "$sys" "$RESET"
		else
			printf '  %s%s%s\n' "$ACCENT_TEXT$BOLD" "${word[0]}" "$RESET"
			printf '  %s%s%s\n' "$ACCENT_TEXT$BOLD" "${word[1]}" "$RESET"
			printf '  %s%s%s\n  %s%s%s\n' "$ACCENT_TEXT" "$release" "$RESET" "$DIM" "$sys" "$RESET"
		fi
	fi
	if [ "$DRY_RUN" -eq 1 ]; then
		printf '\n  %s%sDRY RUN%s %s%s nothing on this host will change%s\n' "$YELLOW" "$BOLD" "$RESET" "$YELLOW" "$G_DOT" "$RESET"
	fi
	echo
}

# A rounded box sized to its longest row. Arguments are rows: "label<TAB>value<TAB>color",
# or a bare title. Values are never truncated (the setup token must be copyable).
box() {
	local row label value color width=0 len
	for row in "$@"; do
		if [[ "$row" == *$'\t'* ]]; then
			label=${row%%$'\t'*}; value=${row#*$'\t'}; value=${value%$'\t'*}
			len=$((8 + ${#value}))
		else
			len=${#row}
		fi
		[ "$len" -le "$width" ] || width=$len
	done
	local rule="" i
	for ((i = 0; i < width + 4; i++)); do rule+="$B_H"; done
	printf '  %s%s%s%s%s\n' "$ACCENT" "$B_TL" "$rule" "$B_TR" "$RESET"
	for row in "$@"; do
		if [[ "$row" == *$'\t'* ]]; then
			label=${row%%$'\t'*}; value=${row#*$'\t'}; color=${value##*$'\t'}; value=${value%$'\t'*}
			printf '  %s%s%s  %s%-7s%s %s%s%s%*s  %s%s%s\n' "$ACCENT" "$B_V" "$RESET" \
				"$DIM" "$label" "$RESET" "$color" "$value" "$RESET" $((width - 8 - ${#value})) "" "$ACCENT" "$B_V" "$RESET"
		elif [ -z "$row" ]; then
			printf '  %s%s%s%*s%s%s%s\n' "$ACCENT" "$B_V" "$RESET" $((width + 4)) "" "$ACCENT" "$B_V" "$RESET"
		else
			printf '  %s%s%s  %s%s%s%*s  %s%s%s\n' "$ACCENT" "$B_V" "$RESET" "$BOLD" "$row" "$RESET" $((width - ${#row})) "" "$ACCENT" "$B_V" "$RESET"
		fi
	done
	printf '  %s%s%s%s%s\n' "$ACCENT" "$B_BL" "$rule" "$B_BR" "$RESET"
}

usage() {
	cat <<'USAGE'
forgenerd installer. Re-running upgrades the binary in place (data and secrets are kept).

  --version vX.Y.Z    release to install (default: latest)
  --repo owner/name   GitHub repository to download from (default: forgenerd/forgenerd)
  --binary PATH       install a local binary instead of downloading
  --public-panel      open the panel on http://<server-ip>:4367 (default for new installs)
  --private-panel     panel on 127.0.0.1:4367 only
  --no-swap           do not create a swapfile on small machines
  --dry-run           preview the install without changing the host
USAGE
}

cleanup() {
	if [ -n "${tmp:-}" ] && [ -d "$tmp" ]; then rm -rf -- "$tmp"; fi
}
trap cleanup EXIT

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

[[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "invalid GitHub repository: $REPO"
if [ "$VERSION" != "latest" ]; then
	[[ "$VERSION" =~ ^v?[0-9][A-Za-z0-9._-]*$ ]] || die "invalid release version: $VERSION"
fi

# ---------------------------------------------------------------- preflight
banner

[ "$(uname -s)" = "Linux" ] || die "forgenerd runs on Linux only."
case "$(uname -m)" in
	x86_64|amd64) TARGET="x86_64-unknown-linux-musl" ;;
	aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
	*) die "unsupported CPU architecture: $(uname -m)" ;;
esac
if [ "$DRY_RUN" -eq 0 ] && [ "$(id -u)" -ne 0 ]; then
	die "run as root (e.g. with sudo)."
fi
if [ "$DRY_RUN" -eq 0 ]; then
	command -v systemctl >/dev/null || die "systemd is required."
fi

UPGRADE=0
[ -x "$BIN_PATH" ] && UPGRADE=1
# New installs are public so the first login works without an SSH tunnel. Upgrades
# preserve the existing exposure unless the user explicitly chooses a mode.
if [ -z "$PUBLIC_PANEL" ]; then
	PUBLIC_PANEL=1
	if [ "$UPGRADE" -eq 1 ]; then
		PUBLIC_PANEL=0
		if grep -q '^Environment=FORGENERD_LISTEN=0.0.0.0:' "$UNIT_PATH" 2>/dev/null; then PUBLIC_PANEL=1; fi
	fi
fi
LISTEN_LINE=""
if [ "$PUBLIC_PANEL" -eq 1 ]; then LISTEN_LINE="Environment=FORGENERD_LISTEN=0.0.0.0:$PANEL_PORT"; fi
# The panel's update check compares the running version with this repository's releases.
REPO_LINE=""
if [ "$REPO" != "forgenerd/forgenerd" ]; then REPO_LINE="Environment=FORGENERD_REPO=$REPO"; fi

step "Checking Docker Engine"
if [ "$DRY_RUN" -eq 1 ]; then
	detail "would verify Docker Engine, Compose, buildx and git"
	result "previewed"
else
	command -v docker >/dev/null || die "Docker Engine is not installed. Install it first: https://docs.docker.com/engine/install/"
	docker info >/dev/null 2>&1 || die "Docker is installed but not running (or not accessible). Start it: systemctl enable --now docker"
	docker compose version >/dev/null 2>&1 || die "The Docker Compose plugin is missing. Install the docker-compose-plugin package."
	docker buildx version >/dev/null 2>&1 || warn "docker buildx not found: git builds will use the legacy builder."
	command -v git >/dev/null || warn "git is not installed: git-based apps cannot build until it is (apt install git)."
	result "$(docker version --format 'Docker {{.Server.Version}}') $G_DOT $(docker compose version --short 2>/dev/null | sed 's/^/Compose /')"
fi

port_in_use() {
	if command -v ss >/dev/null; then ss -Hltn "sport = :$1" | grep -q .; else return 1; fi
}
step "Checking ports"
if [ "$UPGRADE" -eq 0 ] && [ "$DRY_RUN" -eq 0 ]; then
	for p in 80 443 "$PANEL_PORT"; do
		if port_in_use "$p"; then
			die "port $p is already in use. forgenerd needs 80/443 for its proxy and $PANEL_PORT for the panel."
		fi
	done
	result "80, 443 and $PANEL_PORT are free"
elif [ "$DRY_RUN" -eq 1 ]; then
	detail "would check ports 80, 443 and $PANEL_PORT"
	result "previewed"
else
	result "upgrade: keeping the existing port configuration"
fi

# ---------------------------------------------------------------- swap
mem_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo)
swap_kb=$(awk '/SwapTotal/ {print $2}' /proc/meminfo)
step "Checking swap"
if [ "$CREATE_SWAP" -eq 1 ] && [ "$swap_kb" -eq 0 ] && [ "$mem_kb" -lt 2097152 ] && [ ! -e /swapfile ]; then
	detail "this machine has $((mem_kb / 1024)) MB RAM and no swap"
	detail "image builds need more memory than a 1 GB VPS has (skip with --no-swap)"
	run fallocate -l 2G /swapfile
	run chmod 600 /swapfile
	run mkswap /swapfile
	run swapon /swapfile
	if ! grep -q '^/swapfile ' /etc/fstab; then
		if [ "$DRY_RUN" -eq 1 ]; then detail "$DIM\$ append /swapfile to /etc/fstab$RESET"; else echo '/swapfile none swap sw 0 0' >> /etc/fstab; fi
	fi
	result "2 GB swapfile created"
else
	result "no swap changes needed"
fi

# ---------------------------------------------------------------- binary
step "Installing the forgenerd binary"
tmp=$(mktemp -d)
INSTALLED_VERSION=""
if [ -n "$LOCAL_BINARY" ]; then
	[ -f "$LOCAL_BINARY" ] || die "no such file: $LOCAL_BINARY"
	run cp "$LOCAL_BINARY" "$tmp/forgenerd"
	detail "using local binary $LOCAL_BINARY"
else
	if [ "$DRY_RUN" -eq 0 ]; then
		command -v curl >/dev/null || die "curl is required to download the release."
		command -v sha256sum >/dev/null || die "sha256sum is required to verify the release."
	fi
	if [ "$VERSION" = "latest" ]; then
		base="https://github.com/$REPO/releases/latest/download"
	else
		base="https://github.com/$REPO/releases/download/$VERSION"
	fi
	asset="forgenerd-$TARGET"
	detail "downloading $asset from $REPO ($VERSION)"
	run curl -fsSL --retry 3 -o "$tmp/forgenerd" "$base/$asset"
	run curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
	if [ "$DRY_RUN" -eq 0 ]; then
		expected=$(awk -v a="$asset" '$2 == a || $2 == "*"a {print $1}' "$tmp/SHA256SUMS")
		[ -n "$expected" ] || die "SHA256SUMS has no entry for $asset."
		actual=$(sha256sum "$tmp/forgenerd" | awk '{print $1}')
		[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual)."
		detail "checksum verified"
	fi
fi
if [ "$DRY_RUN" -eq 0 ]; then
	chmod 755 "$tmp/forgenerd"
	INSTALLED_VERSION=$("$tmp/forgenerd" --version 2>/dev/null) || die "the downloaded binary does not run on this machine."
fi
run install -m 0755 "$tmp/forgenerd" "$BIN_PATH"
if [ "$DRY_RUN" -eq 1 ]; then result "previewed"; else result "${INSTALLED_VERSION:-forgenerd} at $BIN_PATH"; fi

# ---------------------------------------------------------------- user and data
step "Preparing the service user"
if ! id "$SERVICE_USER" >/dev/null 2>&1; then
	detail "creating the $SERVICE_USER system user"
	run useradd --system --user-group --home-dir "$DATA_DIR" --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
else
	detail "using the existing $SERVICE_USER system user"
fi
# Docker socket access is root-equivalent; it is what forgenerd exists to use.
run usermod -aG docker "$SERVICE_USER"
run install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$DATA_DIR"
run install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$DATA_DIR/update"
result "$SERVICE_USER user, data in $DATA_DIR"

# ---------------------------------------------------------------- systemd
step "Writing the systemd service"
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
printf '%s\n' "$unit" > "$tmp/forgenerd.service"
run install -m 0644 "$tmp/forgenerd.service" "$UNIT_PATH"

# Updates from the panel. The panel runs unprivileged and cannot replace its own binary,
# so it only writes a release tag to $DATA_DIR/update/request. The path unit then runs
# this root-owned helper, which installs that release with the release's own installer
# (checksum-verified) and records the outcome for the panel. The repository is fixed
# here and the tag is validated: the panel can name a version but nothing else.
updater=$(cat <<EOF
#!/usr/bin/env bash
# Installs the forgenerd release requested from the panel. Run by forgenerd-update.path.
set -uo pipefail
main() {
	local dir="$DATA_DIR/update"
	local request="\$dir/request" log="\$dir/log" result="\$dir/result" version tmp
	[ -f "\$request" ] || exit 0
	version=\$(head -c 64 "\$request" | tr -d '[:space:]')
	rm -f -- "\$request"
	# Never write through links the service user could have planted.
	[ ! -L "\$log" ] || rm -f -- "\$log"
	[ ! -L "\$result" ] || rm -f -- "\$result"
	exec >>"\$log" 2>&1
	finish() {
		printf '%s %s\n' "\$1" "\$version" >"\$result"
		chown $SERVICE_USER:$SERVICE_USER "\$log" "\$result" 2>/dev/null || true
		exit "\$2"
	}
	[[ "\$version" =~ ^v[0-9][A-Za-z0-9.+-]{0,31}\$ ]] || { echo "invalid release version"; finish failed 1; }
	tmp=\$(mktemp -d /tmp/forgenerd-update.XXXXXX) || finish failed 1
	trap 'rm -f -- "\$tmp/install.sh"; rmdir -- "\$tmp"' EXIT
	echo "Downloading the \$version installer from $REPO"
	curl -fsSL --retry 3 -o "\$tmp/install.sh" "https://github.com/$REPO/releases/download/\$version/install.sh" \\
		|| { echo "could not download the installer for \$version"; finish failed 1; }
	NO_COLOR=1 bash "\$tmp/install.sh" --version "\$version" --repo "$REPO" || finish failed 1
	finish ok 0
}
# Parsed in full before it runs, so the installer can replace this file while it runs.
main "\$@"
EOF
)
printf '%s\n' "$updater" > "$tmp/forgenerd-update"
cat > "$tmp/forgenerd-update.service" <<EOF
[Unit]
Description=Install the forgenerd release requested from the panel
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=$UPDATER_PATH
EOF
cat > "$tmp/forgenerd-update.path" <<EOF
[Unit]
Description=Watch for forgenerd updates requested from the panel

[Path]
PathExists=$DATA_DIR/update/request

[Install]
WantedBy=multi-user.target
EOF
run install -d -m 0755 "$(dirname "$UPDATER_PATH")"
run install -m 0755 "$tmp/forgenerd-update" "$UPDATER_PATH"
run install -m 0644 "$tmp/forgenerd-update.service" "$UPDATER_UNIT"
run install -m 0644 "$tmp/forgenerd-update.path" "$UPDATER_WATCH"
run systemctl daemon-reload
if [ "$UPGRADE" -eq 1 ]; then
	run systemctl enable forgenerd
	run systemctl restart forgenerd
else
	run systemctl enable --now forgenerd
fi
run systemctl is-active --quiet forgenerd
run systemctl enable --now forgenerd-update.path
if [ "$DRY_RUN" -eq 1 ]; then result "previewed"; else result "forgenerd.service is active, panel updates on"; fi

# ---------------------------------------------------------------- firewall
# Caddy (in Docker) reaches the panel on the forgenerd-internal gateway address. UFW's
# default INPUT policy drops that traffic, which would break the panel domain.
step "Configuring the firewall"
if [ "$DRY_RUN" -eq 1 ]; then
	detail "would configure UFW for HTTP, HTTPS and the panel"
	result "previewed"
elif command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q 'Status: active'; then
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
	result "UFW allows HTTP, HTTPS and the panel"
else
	result "UFW is not active; no changes"
fi
step_done

# The lookup is only used to make the hand-off URL convenient. It must never make
# an otherwise successful install depend on an external IP-echo service.
PUBLIC_IP=""
if [ "$DRY_RUN" -eq 0 ] && [ "$PUBLIC_PANEL" -eq 1 ] && command -v curl >/dev/null 2>&1; then
	PUBLIC_IP=$(curl -4fsS --max-time 5 https://checkip.amazonaws.com 2>/dev/null | tr -d '[:space:]' || true)
	if [[ ! "$PUBLIC_IP" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
		PUBLIC_IP=""
	fi
fi

# ---------------------------------------------------------------- done
public_host="${PUBLIC_IP:-<server-ip>}"
TOTAL=$(fmt_ms $(($(now_ms) - STARTED_AT)))
T=$'\t'
panel_rows=()
if [ "$PUBLIC_PANEL" -eq 1 ]; then
	panel_rows+=("Panel${T}http://$public_host:$PANEL_PORT${T}$ACCENT_TEXT")
else
	panel_rows+=("Tunnel${T}ssh -L $PANEL_PORT:127.0.0.1:$PANEL_PORT <user>@<server>${T}$ACCENT_TEXT")
	panel_rows+=("Panel${T}http://localhost:$PANEL_PORT${T}$ACCENT_TEXT")
fi

panel_notes() {
	if [ "$PUBLIC_PANEL" -eq 1 ]; then
		printf '  %sAllow inbound TCP %s in your cloud firewall (e.g. Lightsail Networking tab,\n' "$DIM" "$PANEL_PORT"
		printf '  EC2 security group). The panel is plain HTTP until it has a domain: then\n'
		printf '  re-run this installer with --private-panel and close the port.%s\n' "$RESET"
	else
		printf '  %sOr open it in a browser: re-run with --public-panel and allow inbound TCP %s.%s\n' "$DIM" "$PANEL_PORT" "$RESET"
	fi
}

echo
if [ "$DRY_RUN" -eq 1 ]; then
	box "$G_OK Dry run complete: nothing was changed  ($TOTAL)" "" "${panel_rows[@]}"
	echo
	panel_notes
	echo
	exit 0
fi
if [ "$UPGRADE" -eq 1 ]; then
	box "$G_OK ${INSTALLED_VERSION:-forgenerd} upgraded and restarted  ($TOTAL)" "" "${panel_rows[@]}"
	echo
	panel_notes
	echo
	exit 0
fi
token=""
for _ in $(seq 20); do
	token=$(journalctl -u forgenerd --no-pager -n 100 2>/dev/null | grep -A3 'setup token' | grep -oE '[A-Za-z0-9_-]{40,}' | tail -1 || true)
	[ -n "$token" ] && break
	sleep 1
done
box "$G_OK ${INSTALLED_VERSION:-forgenerd} is running  ($TOTAL)" "" \
	"${panel_rows[@]}" \
	"Token${T}${token:-see: journalctl -u forgenerd | grep -A3 'setup token'}${T}$GREEN$BOLD" \
	"Next${T}Settings $G_ARROW set the root domain (https://hero.<domain>)${T}"
echo
panel_notes
cat <<EOF

  ${YELLOW}${BOLD}Back up $DATA_DIR/master.key${RESET}${YELLOW} somewhere safe, away from this server.
  Every stored secret (env vars, S3 keys, tokens) is encrypted with it; without it
  they cannot be recovered.${RESET}

  ${DIM}Logs${RESET}     journalctl -u forgenerd -f
  ${DIM}Upgrade${RESET}  re-run this installer

EOF
