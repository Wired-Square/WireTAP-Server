# TimescaleDB with pgBackRest layered in. pgBackRest is inert unless the
# `backup` compose profile is used (archive_command defaults to /bin/true,
# which recycles WAL without archiving).
# Pinned to what deploy/ ships, so development runs the engine production does.
FROM timescale/timescaledb:2.29.2-pg16
RUN apk add --no-cache pgbackrest
COPY pgbackrest.conf /etc/pgbackrest/pgbackrest.conf
COPY backup-loop.sh /usr/local/bin/backup-loop.sh
RUN chmod +x /usr/local/bin/backup-loop.sh \
    && mkdir -p /var/lib/pgbackrest \
    && chown postgres:postgres /var/lib/pgbackrest /etc/pgbackrest/pgbackrest.conf
