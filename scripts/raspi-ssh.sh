#!/usr/bin/env bash
# Shared client settings; source from the network-facing launchers.
# Explicit environment values take precedence over the optional local config.
scarlet_rpi_init() {
  local configured_host=${SCARLET_RPI_HOST-} configured_user=${SCARLET_RPI_USER-}
  local configured_identity=${SCARLET_RPI_IDENTITY-}
  local config=${SCARLET_RPI_CLIENT_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/scarlet/raspi.env}
  if [[ -r "$config" ]]; then
    . "$config"
  fi
  rpi_host=${configured_host:-${SCARLET_RPI_HOST:-raspberrypi.local}}
  rpi_user=${configured_user:-${SCARLET_RPI_USER:-pi}}
  identity_file=${configured_identity:-${SCARLET_RPI_IDENTITY:-}}
  remote="$rpi_user@$rpi_host"
  ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=3)
  if [[ -n "$identity_file" ]]; then
    ssh_opts+=(-i "$identity_file")
  fi
}
scarlet_rpi_init
