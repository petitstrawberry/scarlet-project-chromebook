#!/usr/bin/env bash
set -euo pipefail

# This helper intentionally changes only live macOS state. It does not edit
# /etc/pf.conf or create a launch daemon. A reboot, or `disable`, removes the
# forwarding/NAT setup.

uplink_ifname="${SCARLET_MAC_UPLINK_IFNAME:-en0}"
pi_ifname="${SCARLET_MAC_PI_IFNAME:-en9}"
pi_subnet="${SCARLET_MAC_PI_SUBNET:-192.168.77.0/24}"
anchor="${SCARLET_MAC_PF_ANCHOR:-com.apple/scarlet-project}"
state_file="${SCARLET_MAC_ROUTING_STATE:-/tmp/scarlet-mac-routing.state}"
rules_file="${SCARLET_MAC_ROUTING_RULES:-/tmp/scarlet-mac-routing.rules}"

sysctl_bin="/usr/sbin/sysctl"
pfctl_bin="/sbin/pfctl"

die() {
  echo "mac-scarlet-routing: $*" >&2
  exit 1
}

require_root() {
  [[ "$(id -u)" -eq 0 ]] || die "must run as root (use sudo or an administrator authorization)"
}

valid_name() {
  [[ "$1" =~ ^[[:alnum:]_.-]+$ && "${#1}" -le 15 ]]
}

validate_config() {
  valid_name "$uplink_ifname" || die "invalid uplink interface: $uplink_ifname"
  valid_name "$pi_ifname" || die "invalid Pi interface: $pi_ifname"
  [[ "$uplink_ifname" != "$pi_ifname" ]] || die "uplink and Pi interfaces must differ"
  [[ "$pi_subnet" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}/[0-9]{1,2}$ ]] \
    || die "invalid Pi subnet: $pi_subnet"
  [[ "$anchor" =~ ^[[:alnum:]_.-]+(/[[:alnum:]_.-]+)+$ ]] \
    || die "invalid PF anchor: $anchor"
  [[ -x "$sysctl_bin" && -x "$pfctl_bin" ]] || die "macOS sysctl/pfctl is unavailable"
}

pf_is_enabled() {
  "$pfctl_bin" -s info 2>/dev/null | grep -qE '^Status:[[:space:]]+Enabled'
}

pf_token_from_output() {
  sed -nE 's/.*[Tt]oken[[:space:]]*[:=][[:space:]]*([^[:space:]]+).*/\1/p' | tail -1
}

read_state() {
  [[ -r "$state_file" ]] || return 1
  # The state file is generated below from constants and validated values.
  # shellcheck disable=SC1090
  . "$state_file"
  [[ "${SCARLET_FORWARDING_BEFORE:-}" =~ ^[01]$ ]] || return 1
  [[ "${SCARLET_PF_ENABLED_BY_US:-}" =~ ^[01]$ ]] || return 1
  return 0
}

write_rules() {
  cat >"$rules_file" <<EOF
nat on $uplink_ifname inet from $pi_subnet to any -> ($uplink_ifname)
pass in quick on $pi_ifname inet from $pi_subnet to any keep state
pass out quick on $uplink_ifname inet from $pi_subnet to any keep state
pass out quick on $pi_ifname inet from any to $pi_subnet keep state
EOF
  chmod 0600 "$rules_file"
}

flush_anchor() {
  "$pfctl_bin" -a "$anchor" -F all >/dev/null 2>&1 || true
}

enable_routing() {
  validate_config
  if read_state; then
    echo "mac-scarlet-routing: already enabled (state=$state_file)"
    return 0
  fi

  local forwarding_before
  forwarding_before=$("$sysctl_bin" -n net.inet.ip.forwarding)
  [[ "$forwarding_before" =~ ^[01]$ ]] || die "unexpected forwarding state: $forwarding_before"

  local pf_enabled_by_us=0
  local pf_token=""
  if ! pf_is_enabled; then
    local pf_enable_output
    pf_enable_output=$("$pfctl_bin" -E 2>&1)
    pf_enabled_by_us=1
    pf_token=$(printf '%s\n' "$pf_enable_output" | pf_token_from_output)
  fi

  write_rules
  if ! "$pfctl_bin" -a "$anchor" -f "$rules_file"; then
    [[ -n "$pf_token" ]] && "$pfctl_bin" -X "$pf_token" || true
    rm -f -- "$rules_file"
    die "failed to load PF rules"
  fi

  if ! "$sysctl_bin" -w net.inet.ip.forwarding=1 >/dev/null; then
    flush_anchor
    [[ -n "$pf_token" ]] && "$pfctl_bin" -X "$pf_token" || true
    rm -f -- "$rules_file"
    die "failed to enable IPv4 forwarding"
  fi

  umask 077
  {
    printf 'SCARLET_FORWARDING_BEFORE=%q\n' "$forwarding_before"
    printf 'SCARLET_PF_ENABLED_BY_US=%q\n' "$pf_enabled_by_us"
    printf 'SCARLET_PF_TOKEN=%q\n' "$pf_token"
    printf 'SCARLET_PF_ANCHOR=%q\n' "$anchor"
  } >"$state_file"
  echo "mac-scarlet-routing: enabled on $pi_ifname -> $uplink_ifname"
}

disable_routing() {
  require_root
  if ! read_state; then
    echo "mac-scarlet-routing: no saved state; nothing to disable"
    return 0
  fi

  local active_anchor="${SCARLET_PF_ANCHOR:-$anchor}"
  anchor="$active_anchor"
  flush_anchor
  if [[ -n "${SCARLET_PF_TOKEN:-}" ]]; then
    "$pfctl_bin" -X "$SCARLET_PF_TOKEN" >/dev/null 2>&1 || true
  fi
  "$sysctl_bin" -w "net.inet.ip.forwarding=${SCARLET_FORWARDING_BEFORE}" >/dev/null || true
  rm -f -- "$state_file" "$rules_file"
  echo "mac-scarlet-routing: disabled (anchor=$active_anchor)"
}

status_routing() {
  require_root
  echo "forwarding: $("$sysctl_bin" -n net.inet.ip.forwarding)"
  if pf_is_enabled; then
    echo 'pf: enabled'
  else
    echo 'pf: disabled'
  fi
  echo "anchor: $anchor"
  "$pfctl_bin" -a "$anchor" -sr 2>/dev/null || true
  "$pfctl_bin" -a "$anchor" -sn 2>/dev/null || true
  if [[ -r "$state_file" ]]; then
    echo "state: $state_file"
  else
    echo 'state: inactive'
  fi
}

usage() {
  cat >&2 <<'EOF'
usage: mac-scarlet-routing {enable|disable|status}

enable   Temporarily enable IPv4 forwarding and PF NAT from en9/192.168.77.0/24
         to en0. No persistent macOS configuration is changed.
disable  Remove only the Scarlet PF anchor and restore the previous forwarding
         value/PF enable reference.
status   Show current forwarding, PF, and anchor state.
EOF
}

require_root
case "${1:-}" in
  enable) enable_routing ;;
  disable) disable_routing ;;
  status) status_routing ;;
  *) usage; exit 2 ;;
esac
