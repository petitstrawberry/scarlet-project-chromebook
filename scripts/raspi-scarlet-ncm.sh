#!/usr/bin/env bash
set -euo pipefail

config_env="${SCARLET_CONFIG_FILE:-/etc/default/scarlet-gadget}"
if [[ -r "$config_env" ]]; then
  # The file is installed root-owned by the deployment script.
  . "$config_env"
fi

runtime_dir="${SCARLET_RUNTIME_DIR:-/run/scarlet}"
ifname="${SCARLET_NCM_IFNAME:-usb0}"
pi_address="${SCARLET_NCM_PI_ADDRESS:-192.168.88.1/30}"
network_cidr="${SCARLET_NCM_NETWORK:-192.168.88.0/30}"
uplink_ifname="${SCARLET_NCM_UPLINK_IFNAME:-eth0}"
uplink_gateway="${SCARLET_NCM_UPLINK_GATEWAY:-}"
uplink_route_metric="${SCARLET_NCM_UPLINK_ROUTE_METRIC:-100}"
enable_routing="${SCARLET_NCM_ENABLE_ROUTING:-1}"
enable_nat="${SCARLET_NCM_ENABLE_NAT:-1}"
configure_network="${SCARLET_NCM_CONFIGURE_NETWORK:-1}"
dhcp_start="${SCARLET_NCM_DHCP_START:-192.168.88.2}"
dhcp_end="${SCARLET_NCM_DHCP_END:-192.168.88.2}"
dhcp_netmask="${SCARLET_NCM_DHCP_NETMASK:-255.255.255.252}"
dhcp_lease="${SCARLET_NCM_DHCP_LEASE:-1h}"
dhcp_dns="${SCARLET_NCM_DHCP_DNS:-}"
dns_proxy="${SCARLET_NCM_DNS_PROXY:-$enable_routing}"
dnsmasq_bin="${SCARLET_NCM_DNSMASQ:-/usr/sbin/dnsmasq}"
nft_bin="${SCARLET_NCM_NFT:-/usr/sbin/nft}"
ip_bin="${SCARLET_NCM_IP:-/usr/sbin/ip}"

nft_table="${SCARLET_NCM_NFT_TABLE:-scarlet_ncm}"
dnsmasq_config="$runtime_dir/dnsmasq-ncm.conf"
dnsmasq_leasefile="$runtime_dir/dnsmasq-ncm.leases"
dnsmasq_pidfile="$runtime_dir/dnsmasq-ncm.pid"
nft_config="$runtime_dir/nft-ncm.conf"
forward_backup="$runtime_dir/ip-forward.previous"
sysctl_path="/proc/sys/net/ipv4/ip_forward"

die() {
  echo "scarlet-ncm: $*" >&2
  exit 1
}

require_root() {
  [[ "$(id -u)" -eq 0 ]] || die "must run as root"
}

function_requested() {
  local requested="$1"
  local function_name
  local function_spec="${SCARLET_GADGET_FUNCTIONS:-mass_storage,ncm}"
  IFS=',' read -r -a configured_functions <<< "$function_spec"
  for function_name in "${configured_functions[@]}"; do
    [[ "$function_name" == "$requested" ]] && return 0
  done
  return 1
}

valid_ipv4() {
  local address="$1"
  local octet
  local -a octets=()
  IFS='.' read -r -a octets <<< "$address"
  [[ "${#octets[@]}" -eq 4 ]] || return 1
  for octet in "${octets[@]}"; do
    [[ "$octet" =~ ^[0-9]{1,3}$ ]] || return 1
    ((10#$octet <= 255)) || return 1
  done
}

valid_cidr() {
  local cidr="$1"
  local address="${cidr%%/*}"
  local prefix="${cidr#*/}"
  [[ "$cidr" == */* && "$prefix" =~ ^[0-9]{1,2}$ ]] || return 1
  ((10#$prefix <= 32)) || return 1
  valid_ipv4 "$address"
}

valid_interface_name() {
  local name="$1"
  [[ "$name" =~ ^[[:alnum:]_.-]+$ && "${#name}" -le 15 ]]
}

validate_config() {
  local pi_ip="${pi_address%%/*}"
  local pi_prefix="${pi_address#*/}"
  valid_interface_name "$ifname" || die "invalid NCM interface name: $ifname"
  valid_interface_name "$uplink_ifname" || die "invalid uplink interface name: $uplink_ifname"
  [[ "$ifname" != "$uplink_ifname" ]] || die "NCM and uplink interfaces must differ"
  [[ "$pi_address" == */* && "$pi_prefix" =~ ^[0-9]{1,2}$ ]] || die "invalid Pi NCM address: $pi_address"
  ((10#$pi_prefix <= 32)) || die "invalid Pi NCM prefix: $pi_address"
  valid_ipv4 "$pi_ip" || die "invalid Pi NCM address: $pi_address"
  valid_cidr "$network_cidr" || die "invalid NCM network: $network_cidr"
  if [[ -n "$uplink_gateway" ]]; then
    valid_ipv4 "$uplink_gateway" || die "invalid uplink gateway: $uplink_gateway"
    [[ "$uplink_route_metric" =~ ^[0-9]+$ ]] || die "invalid uplink route metric: $uplink_route_metric"
    ((10#$uplink_route_metric <= 4294967295)) || die "uplink route metric is too large: $uplink_route_metric"
  fi
  valid_ipv4 "$dhcp_start" || die "invalid DHCP start address: $dhcp_start"
  valid_ipv4 "$dhcp_end" || die "invalid DHCP end address: $dhcp_end"
  valid_ipv4 "$dhcp_netmask" || die "invalid DHCP netmask: $dhcp_netmask"
  [[ "$enable_routing" == 0 || "$enable_routing" == 1 ]] || die "SCARLET_NCM_ENABLE_ROUTING must be 0 or 1"
  [[ "$enable_nat" == 0 || "$enable_nat" == 1 ]] || die "SCARLET_NCM_ENABLE_NAT must be 0 or 1"
  [[ "$configure_network" == 0 || "$configure_network" == 1 ]] || die "SCARLET_NCM_CONFIGURE_NETWORK must be 0 or 1"
  [[ "$dns_proxy" == 0 || "$dns_proxy" == 1 ]] || die "SCARLET_NCM_DNS_PROXY must be 0 or 1"
  if [[ -n "$dhcp_dns" ]]; then
    [[ "$dhcp_dns" =~ ^[0-9.,[:space:]]+$ ]] || die "SCARLET_NCM_DHCP_DNS must be a comma-separated IPv4 list"
  fi
  [[ -x "$ip_bin" ]] || die "ip command is unavailable: $ip_bin"
  if [[ "$enable_nat" == 1 ]]; then
    [[ -x "$nft_bin" ]] || die "nft command is unavailable: $nft_bin"
  fi
  if [[ "$dns_proxy" == 1 || "$enable_routing" == 1 ]]; then
    [[ -x "$dnsmasq_bin" ]] || die "dnsmasq is unavailable: $dnsmasq_bin"
  fi
}

wait_for_interface() {
  local attempts="${SCARLET_NCM_INTERFACE_WAIT_SEC:-30}"
  [[ "$attempts" =~ ^[0-9]+$ ]] || die "SCARLET_NCM_INTERFACE_WAIT_SEC must be an integer"
  for ((second = 0; second < attempts * 10; second++)); do
    [[ -e "/sys/class/net/$ifname" ]] && return 0
    sleep 0.1
  done
  die "NCM interface did not appear: $ifname"
}

configure_interface() {
  wait_for_interface
  "$ip_bin" link set dev "$ifname" up
  "$ip_bin" addr replace "$pi_address" dev "$ifname"
}

configure_uplink_route() {
  [[ -n "$uplink_gateway" ]] || return 0
  [[ -e "/sys/class/net/$uplink_ifname" ]] || die "uplink interface is unavailable: $uplink_ifname"
  "$ip_bin" route replace default via "$uplink_gateway" dev "$uplink_ifname" metric "$uplink_route_metric"
}

enable_forwarding() {
  [[ "$enable_routing" == 1 ]] || return 0
  [[ -w "$sysctl_path" ]] || die "IPv4 forwarding sysctl is unavailable"
  install -d -m 0755 "$runtime_dir"
  if [[ ! -r "$forward_backup" ]]; then
    tr -d '[:space:]' <"$sysctl_path" >"$forward_backup"
  fi
  printf '1\n' >"$sysctl_path"
}

disable_forwarding() {
  [[ -r "$forward_backup" ]] || return 0
  local previous
  previous=$(tr -d '[:space:]' <"$forward_backup" 2>/dev/null || true)
  if [[ "$previous" == 0 || "$previous" == 1 ]]; then
    printf '%s\n' "$previous" >"$sysctl_path" 2>/dev/null || true
  fi
  rm -f -- "$forward_backup"
}

apply_nft_rules() {
  [[ "$enable_nat" == 1 ]] || return 0
  [[ -e "/sys/class/net/$uplink_ifname" ]] || die "uplink interface is unavailable: $uplink_ifname"
  install -d -m 0755 "$runtime_dir"
  cat >"$nft_config" <<EOF
table ip $nft_table {
  chain forward {
    type filter hook forward priority filter; policy accept;
    iifname "$ifname" oifname "$uplink_ifname" accept
    iifname "$uplink_ifname" oifname "$ifname" ct state established,related accept
  }
  chain postrouting {
    type nat hook postrouting priority srcnat; policy accept;
    oifname "$uplink_ifname" ip saddr $network_cidr masquerade
  }
}
EOF
  "$nft_bin" delete table ip "$nft_table" 2>/dev/null || true
  "$nft_bin" -f "$nft_config"
}

remove_nft_rules() {
  if [[ -x "$nft_bin" ]]; then
    "$nft_bin" delete table ip "$nft_table" 2>/dev/null || true
  fi
  rm -f -- "$nft_config"
}

write_dnsmasq_config() {
  local pi_ip="${pi_address%%/*}"
  local temp_config="$dnsmasq_config.part"
  install -d -m 0755 "$runtime_dir"
  {
    echo '# Generated by scarlet-ncm; do not edit.'
    echo "interface=$ifname"
    echo 'bind-dynamic'
    echo 'dhcp-authoritative'
    # There is a single /30 lease and the Scarlet client has no address yet;
    # avoid dnsmasq's pre-offer ARP probe, which some USB-NCM links do not
    # answer until the lease has been installed.
    echo 'no-ping'
    echo "dhcp-range=$dhcp_start,$dhcp_end,$dhcp_netmask,$dhcp_lease"
    echo "dhcp-option=3,$pi_ip"
    if [[ -n "$dhcp_dns" && "$dns_proxy" != 1 ]]; then
      echo "dhcp-option=6,$dhcp_dns"
    fi
    echo 'no-hosts'
    if [[ "$dns_proxy" == 1 ]]; then
      echo "listen-address=$pi_ip"
      echo "dhcp-option=6,$pi_ip"
      if [[ -n "$dhcp_dns" ]]; then
        IFS=',' read -r -a upstream_dns <<< "$dhcp_dns"
        for dns_server in "${upstream_dns[@]}"; do
          echo "server=$dns_server"
        done
      fi
    else
      # This Pi is commonly connected to an isolated host LAN without a
      # default route. Run DHCP-only unless an operator explicitly enables a
      # DNS proxy, so clients are never advertised a dead resolver.
      echo 'port=0'
    fi
    echo "dhcp-leasefile=$dnsmasq_leasefile"
    echo 'log-facility=-'
  } >"$temp_config"
  chmod 0644 "$temp_config"
  mv -f -- "$temp_config" "$dnsmasq_config"
}

stop_dnsmasq() {
  local pid=''
  if [[ -r "$dnsmasq_pidfile" ]]; then
    pid=$(tr -d '[:space:]' <"$dnsmasq_pidfile" 2>/dev/null || true)
  fi
  if [[ "$pid" =~ ^[0-9]+$ ]] && [[ "$pid" -gt 1 ]] && kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    for _ in {1..30}; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL "$pid" 2>/dev/null || true
  fi
  rm -f -- "$dnsmasq_pidfile" "$dnsmasq_leasefile"
}

setup() {
  function_requested ncm || {
    echo 'scarlet-ncm: NCM is disabled in SCARLET_GADGET_FUNCTIONS'
    return 0
  }
  validate_config
  if [[ "$configure_network" == 1 ]]; then
    configure_interface
  else
    wait_for_interface
  fi
  configure_uplink_route
  enable_forwarding
  apply_nft_rules
  write_dnsmasq_config
}

run_daemon() {
  function_requested ncm || return 0
  setup
  stop_dnsmasq
  exec "$dnsmasq_bin" --keep-in-foreground --conf-file="$dnsmasq_config" --pid-file="$dnsmasq_pidfile"
}

stop() {
  stop_dnsmasq
  remove_nft_rules
  disable_forwarding
}

status() {
  if ! function_requested ncm; then
    echo 'ncm=disabled'
    return 0
  fi
  validate_config
  printf 'ncm_ifname=%s\n' "$ifname"
  printf 'ncm_pi_address=%s\n' "$pi_address"
  printf 'ncm_network=%s\n' "$network_cidr"
  printf 'ncm_uplink=%s\n' "$uplink_ifname"
  printf 'ncm_uplink_gateway=%s\n' "${uplink_gateway:-none}"
  printf 'routing=%s\n' "$enable_routing"
  printf 'nat=%s\n' "$enable_nat"
  printf 'configure_network=%s\n' "$configure_network"
  printf 'dnsmasq=%s\n' "$( [[ -r "$dnsmasq_pidfile" ]] && echo running || echo stopped )"
  if [[ -x "$ip_bin" && -e "/sys/class/net/$ifname" ]]; then
    "$ip_bin" -4 addr show dev "$ifname" | sed -n '1,8p'
  fi
  if [[ -x "$nft_bin" ]]; then
    "$nft_bin" list table ip "$nft_table" 2>/dev/null || true
  fi
}

usage() {
  cat >&2 <<'EOF'
usage: scarlet-ncm {run|setup|stop|status}

run    Configure USB-NCM, forwarding/NAT, and run the DHCP server.
setup  Configure the link and write the dnsmasq/nftables state, but do not
       start dnsmasq.
stop   Stop the DHCP server and remove this service's forwarding/NAT rules.
status Show the configured USB-NCM gateway and service state.
EOF
}

require_root
case "${1:-}" in
  run) run_daemon ;;
  setup) setup ;;
  stop) stop ;;
  status) status ;;
  *) usage; exit 2 ;;
esac
