# Brook SFU: Janus WebRTC server, VideoRoom plugin only.
#
# Built from source because no maintained prebuilt image exists: the image Brook
# referenced (canyan/janus-gateway) is Janus 1.1.4 from May 2023 with 2023-era
# OpenSSL/libsrtp baked in, and Debian dropped the janus package in trixie.
# Building on Debian stable means the crypto libraries Janus links against
# (OpenSSL, libsrtp2, libnice) come from Debian and get its security updates on
# every rebuild.
#
# Only what Brook uses is compiled: the VideoRoom plugin and the WebSocket
# transport that `api` talks to. Every other plugin, transport, event handler and
# logger is off; each one is attack surface nobody would be watching.

FROM debian:trixie-slim AS build
# Pin the commit, not just the tag: a tag can be moved, a commit hash cannot.
ARG JANUS_TAG=v1.4.2
ARG JANUS_COMMIT=0a24110ae55a172c4293749b763dbb66a138f9ec
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential autoconf automake libtool pkg-config gengetopt git ca-certificates \
        libnice-dev libsrtp2-dev libwebsockets-dev libjansson-dev libconfig-dev \
        libssl-dev libglib2.0-dev libopus-dev libogg-dev zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch "$JANUS_TAG" https://github.com/meetecho/janus-gateway.git /src \
    && test "$(git -C /src rev-parse HEAD)" = "$JANUS_COMMIT"
WORKDIR /src
RUN sh autogen.sh \
    && ./configure --prefix=/opt/janus \
        --disable-docs \
        --disable-all-plugins --enable-plugin-videoroom \
        --disable-all-transports --enable-websockets \
        --disable-all-handlers --disable-all-loggers \
    && make -j"$(nproc)" && make install

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        libnice10 libsrtp2-1 libwebsockets19t64 libjansson4 libconfig11 \
        libssl3t64 libglib2.0-0t64 libopus0 libogg0 zlib1g \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin janus
COPY --from=build /opt/janus /opt/janus
COPY janus.jcfg janus.plugin.videoroom.jcfg janus.transport.websockets.jcfg /opt/janus/etc/janus/
COPY entrypoint.sh /usr/local/bin/brook-sfu
USER janus
# 8188: Janus WebSocket API, internal network only (never published; see compose).
EXPOSE 8188
ENTRYPOINT ["/usr/local/bin/brook-sfu"]
