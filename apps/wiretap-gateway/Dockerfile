# The wiretap-gateway ApplianceOS app: wiretap-backend, its own Postgres and
# TimescaleDB, the capture daemon forwarding to it, and its web UI, in one
# image. Built by make-image.sh from the context it stages.

# Pinned like the compose files; the schema needs 2.28.1 or later.
ARG TIMESCALEDB_VERSION=2.29.2

FROM --platform=$BUILDPLATFORM node:22-bookworm-slim AS admin-ui
WORKDIR /ui
COPY admin-ui/package.json admin-ui/package-lock.json ./
RUN npm ci
COPY admin-ui/ ./
RUN npm run build

# The upstream engine, cut down to the server, its client tools and this one
# TimescaleDB, then copied out: a derived image would inherit its VOLUME.
FROM timescale/timescaledb:${TIMESCALEDB_VERSION}-pg16 AS postgres
ARG TIMESCALEDB_VERSION
RUN set -eu; \
    cd /usr/local; \
    rm -rf include lib/*.a lib/libecpg* lib/libpgtypes* lib/pkgconfig \
        lib/postgresql/pgxs lib/postgresql/bitcode lib/postgresql/llvmjit*; \
    find bin -type f ! -name postgres ! -name initdb ! -name pg_isready \
        ! -name pg_controldata ! -name psql ! -name 'pg_dump*' ! -name pg_restore -delete; \
    cd lib/postgresql; \
    rm -f ./*plperl* ./*plpython* ./pltcl* vector*; \
    find . -name 'timescaledb-*.so' ! -name "*-${TIMESCALEDB_VERSION}.so" -delete; \
    cd /usr/local/share/postgresql/extension; \
    rm -f ./*plperl* ./*plpython* ./pltcl* vector*; \
    find . -name 'timescaledb--*' ! -name "timescaledb--${TIMESCALEDB_VERSION}.sql" -delete; \
    scanelf --needed --nobanner --format '%n#p' --recursive /usr/local \
        | tr ',' '\n' | sort -u \
        | awk 'system("[ -e /usr/local/lib/" $1 " ]") == 0 { next } { print "so:" $1 }' > /rundeps

# buildx's transfer of the context drops a file capability, so it is set here.
FROM --platform=$BUILDPLATFORM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS capabilities
RUN apt-get -o APT::Update::Error-Mode=any update && apt-get install -y --no-install-recommends libcap2-bin
COPY --chmod=0755 wiretap-server /usr/bin/wiretap-server
RUN setcap cap_net_raw,cap_net_bind_service+ep /usr/bin/wiretap-server

FROM alpine:3.23.5
# /run/wiretap-server is root's and sticky, so no other uid in group 0 can swap
# a per-uid socket directory.
RUN --mount=type=bind,from=postgres,source=/rundeps,target=/rundeps \
    apk add --no-cache $(cat /rundeps) icu-data-full nss_wrapper tzdata \
        bash can-utils curl iproute2 less procps strace \
    && install -d -o 10001 -g 0 -m 0770 /data /run/wiretap \
    && install -d -m 1770 /run/wiretap-server
COPY --from=postgres /usr/local /usr/local
COPY --from=admin-ui /ui/dist /usr/share/wiretap-admin
COPY --from=capabilities /usr/bin/wiretap-server /usr/bin/
COPY --chmod=0755 wiretap-server-ui /usr/bin/
COPY wiretap-backend entrypoint.sh /usr/local/bin/
COPY ui/ /usr/share/wiretap-server-ui/ui/
COPY wiretap-server.toml /etc/wiretap-server/

ENV LANG=en_US.utf8 \
    WIRETAP_PG_HOST=/run/wiretap \
    WIRETAP_PG_USER=postgres \
    WIRETAP_PG_PASSWORD_FILE=/data/secrets/postgres-password \
    WIRETAP_ADMIN_KEY_FILE=/data/secrets/admin-key \
    WIRETAP_INGEST_KEY_FILE=/data/secrets/ingest-key

ARG VERSION
ARG MANIFEST
ARG WSLIB_BUILD_ID=
LABEL com.wiredsquare.appliance.manifest="${MANIFEST}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.source=https://github.com/Wired-Square/WireTAP-Server \
      org.opencontainers.image.revision="${WSLIB_BUILD_ID}"

VOLUME /data
EXPOSE 8423 9323
USER 10001:0
# For plain Docker; ApplianceOS probes healthz over the app socket instead.
HEALTHCHECK --interval=15s --timeout=5s --retries=3 --start-period=120s --start-interval=2s \
    CMD p="${WIRETAP_BASE_PATH#/}"; wget -q -O /dev/null "http://127.0.0.1:8423/${p:+${p%/}/}healthz" || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
