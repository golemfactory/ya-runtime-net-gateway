#!/bin/sh

IFACE=proxy

set -e

test "$(id -u)" -eq 0 || exec sudo "$0" "$@"
test -n "$SUDO_USER"

ip tuntap add "$IFACE" mode tap user "$SUDO_USER" || :
ip link set dev "$IFACE" up
ip addr add 10.5.6.254/24 dev "$IFACE"
