#!/bin/sh
# Deployment-specific values arrive as env vars and become Janus CLI flags, so the
# .jcfg files stay static. Janus config files cannot read the environment.
set -eu
: "${JANUS_API_SECRET:?set JANUS_API_SECRET (shared with api only)}"
: "${JANUS_PUBLIC_IP:?set JANUS_PUBLIC_IP: the address clients reach for media}"
: "${JANUS_RTP_PORTS:=20000-20099}"
# --nat-1-1: Janus runs on a Docker bridge network and would otherwise advertise
#   its container IP (172.x), unreachable from clients. Advertise the host address
#   the RTP ports are published on instead. (Janus drops the container address
#   from candidates by default when --nat-1-1 is set; --keep-private-host would
#   keep it, which we don't want.)
# --ice-lite: an SFU with a directly reachable address never needs to probe; this
#   halves ICE work and is what media servers conventionally run.
exec /opt/janus/bin/janus \
    --configs-folder=/opt/janus/etc/janus \
    --apisecret="$JANUS_API_SECRET" \
    --nat-1-1="$JANUS_PUBLIC_IP" \
    --ice-lite \
    --rtp-port-range="$JANUS_RTP_PORTS" \
    "$@"
