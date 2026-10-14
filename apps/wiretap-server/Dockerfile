# The wiretap-server app: the capture daemon and its web UI in one image, for
# plain Docker and for ApplianceOS. Built by make-image.sh from the context it
# stages.

# buildx's transfer of the context drops a file capability, so it is set here.
FROM --platform=$BUILDPLATFORM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS capabilities
RUN apt-get -o APT::Update::Error-Mode=any update && apt-get install -y --no-install-recommends libcap2-bin
COPY --chmod=0755 wiretap-server /usr/bin/wiretap-server
RUN setcap cap_net_raw,cap_net_bind_service+ep /usr/bin/wiretap-server

FROM alpine:3.23.5
RUN install -d -o 10001 -g 0 -m 0770 /data /run/wiretap-server
COPY --from=capabilities /usr/bin/wiretap-server /usr/bin/
COPY --chmod=0755 wiretap-server-ui /usr/bin/
COPY --chmod=0755 entrypoint.sh /usr/local/bin/
COPY ui/ /usr/share/wiretap-server-ui/ui/
COPY wiretap-server.toml /etc/wiretap-server/

# SQLite's temporary files go in the data volume.
ENV STATE_DIRECTORY=/data \
    TMPDIR=/data \
    CONFIGURATION_DIRECTORY=/etc/wiretap-server-ui

ARG VERSION
ARG MANIFEST
ARG WIRETAP_BUILD_ID=
LABEL com.wiredsquare.appliance.manifest="${MANIFEST}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.source=https://github.com/Wired-Square/WireTAP-Server \
      org.opencontainers.image.revision="${WIRETAP_BUILD_ID}"

VOLUME /data
USER 10001:0
# For plain Docker; ApplianceOS probes healthz over the app socket instead.
HEALTHCHECK --interval=15s --timeout=5s --retries=3 --start-period=60s --start-interval=2s \
    CMD wget -q -O /dev/null --no-check-certificate "https://127.0.0.1:${WIRETAP_SERVER_UI_HTTPS_PORT:-8443}/healthz" || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
