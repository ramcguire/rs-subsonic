# rs-subsonic with SQLite and Postgres support. Put rs-subsonic.toml in /config
# (or configure with RSUB_* variables); the database, identity.jsonl and
# secret.key live in /data.

# Dependencies are built from cargo-chef's recipe in their own layer, so a
# change to rs-subsonic's code rebuilds only rs-subsonic.
FROM lukemathwalker/cargo-chef:latest-rust-1.98-slim-trixie AS chef
WORKDIR /src

FROM chef AS plan
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=plan /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --features postgres --bin rs-subsonic --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --features postgres --bin rs-subsonic \
    && mkdir -p /out/config /out/data \
    && cp target/release/rs-subsonic /out/rs-subsonic

# glibc, libgcc and CA certificates (rustls verifies Plex's certificate against
# them), no shell; runs as `nonroot` (uid 65532).
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build --chown=nonroot:nonroot /out/config /config
COPY --from=build --chown=nonroot:nonroot /out/data /data
COPY --from=build /out/rs-subsonic /usr/local/bin/rs-subsonic
WORKDIR /config
ENV RSUB_SERVER_DATA_DIR=/data
VOLUME /data
EXPOSE 4533
ENTRYPOINT ["rs-subsonic"]
CMD ["serve"]
