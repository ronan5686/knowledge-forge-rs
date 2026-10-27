# PostgreSQL 16.15-bookworm, official multi-platform image resolved 2026-10-27.
FROM postgres:16.15-bookworm@sha256:d81f7b51697014367236d07ec58b54153fc28f828b72bf247fe5c7487520bcb0 AS vector-build
ARG PGVECTOR_COMMIT=f37c13f68b57d2c3472b2214fbcff699d6d34876
RUN apt-get update && apt-get install -y --no-install-recommends build-essential ca-certificates git postgresql-server-dev-16 \
    && rm -rf /var/lib/apt/lists/* \
    && git init /build/vector \
    && git -C /build/vector remote add origin https://github.com/pgvector/pgvector.git \
    && git -C /build/vector fetch --depth=1 origin "$PGVECTOR_COMMIT" \
    && git -C /build/vector checkout --detach FETCH_HEAD \
    && test "$(git -C /build/vector rev-parse HEAD)" = "$PGVECTOR_COMMIT" \
    && make -C /build/vector OPTFLAGS="" \
    && make -C /build/vector install

FROM postgres:16.15-bookworm@sha256:d81f7b51697014367236d07ec58b54153fc28f828b72bf247fe5c7487520bcb0
LABEL org.opencontainers.image.title="Knowledge Forge PostgreSQL" \
      org.opencontainers.image.version="16.15-pgvector0.8.7" \
      org.opencontainers.image.source="https://github.com/pgvector/pgvector"
COPY --from=vector-build /usr/lib/postgresql/16/lib/vector.so /usr/lib/postgresql/16/lib/vector.so
COPY --from=vector-build /usr/share/postgresql/16/extension/vector* /usr/share/postgresql/16/extension/
