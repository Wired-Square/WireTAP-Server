# The Python oracle, containerised for the Stage 6 parallel run: it reads the
# same buses as the packaged Rust daemon and writes to its own database, so the
# two archives can be diffed frame for frame on live hardware.
#
# Retires with the oracle at Stage 7. Nothing ships this.
#
#   docker build -t wiretap-oracle:parallel tools/oracle
#
# `--network host` is not optional when running it: SocketCAN interfaces are
# network devices and live in a network namespace, so a bridged container sees
# no can0. See parallel-diff.sh's header for the rest of the invocation.
FROM python:3.12-slim

# Neither is needed to capture: pyroute2 is guarded by a try/except at module
# scope and used only by the bitrate query, and psycopg2 is imported inside
# PostgresWriter._connect, so a forward-only run never loads it. Installed
# anyway so the container is not a different program from the host one — and
# installed FROM requirements.txt rather than by repeating its constraints, so
# that claim is true by construction instead of by two lists agreeing.
#
# A container run has been observed reporting the 500 kbit/s fallback rather
# than the real rate. The cause is not established and it is NOT a missing
# capability — packaging/wiretap-server.service records that the netlink bitrate
# query needs none. It reaches the startup log and any GVRET client, never the
# database, so it does not affect the diff; the parallel run attaches no client.
COPY requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt

COPY wiretap-server.py /app/wiretap-server.py
WORKDIR /app
ENTRYPOINT ["python3", "/app/wiretap-server.py"]
