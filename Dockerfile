# Golem in a container: the binary, the pinned engine baked in, and the
# service that owns the engine as PID 1.
#
#   docker build --build-arg GPU_BACKEND=cuda -t golem:cuda .
#   docker build --build-arg GPU_BACKEND=rocm -t golem:rocm .
#
# Docker is packaging, not a mode (docs/architecture.md): one image per GPU
# vendor from this one file, with the adapter's `build` pin deciding which
# engine release `golem install` bakes in - the same pin the eval gate
# adopted, so there is no second one to keep in step. The GPU wiring stays
# the runtime's business (compose.yml): `gpus: all` for CUDA, /dev/kfd +
# /dev/dri for ROCm.

# Both args are global on purpose: an ARG is only usable in FROM lines when
# it is declared before the first one, and GPU_BACKEND picks the runtime
# stage. Inside a stage it has to be redeclared to be used again.
ARG RUST_IMAGE=rust:1-slim
ARG GPU_BACKEND=cuda

# The binary. Plain `cargo build --release`, exactly what CI does on all
# three platforms: the workspace is pure Rust and needs no system build
# dependencies beyond cargo.
FROM ${RUST_IMAGE} AS build
WORKDIR /src
# golem.toml.example, the presets, the library and the default adapter are
# compiled in with include_str!(), resolved relative to the sources - so the
# layout here has to match the checkout's. The console page is the crate's
# own (crates/golem/console/).
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY golem.toml.example golem.toml.example
COPY config config
RUN cargo build --release

# One runtime per vendor; the final stage picks between them. The CUDA tag
# is pinned to a full patch version - the floating 13.4 tag does not exist
# on the hub - and the engine release it will run names its own CUDA (13.4)
# in the asset the adapter matches.
FROM nvidia/cuda:13.4.1-runtime-ubuntu24.04 AS runtime-cuda
FROM rocm/dev-ubuntu-24.04 AS runtime-rocm

ARG GPU_BACKEND=cuda
FROM runtime-${GPU_BACKEND} AS runtime
ARG GPU_BACKEND

# curl is what fetches the pinned engine from GitHub (release.rs uses it
# because the binary has no TLS), and the base images ship tar but not
# curl. The healthcheck uses curl too. libgomp1 is the one library the
# llama.cpp release needs that neither base image carries.
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates libgomp1 \
    && rm -rf /var/lib/apt/lists/*

# The home: everything the binary owns lives under it (crates/README.md).
ENV GOLEM_HOME=/golem
WORKDIR /golem
COPY --from=build /src/target/release/golem /usr/local/bin/golem
# The adapter and the presets; mount a volume over /golem/config to serve
# your own.
COPY config /golem/config

# The container's configuration: the template, with the two keys a
# container has to differ on. The engine binds 0.0.0.0 because the port is
# published rather than loopback, and the backend is a build decision rather
# than a detection because no card exists at build time. The template
# already serves the image's own presets. compose.yml mounts the host's
# golem.toml over this; a bare `docker run` is served by it.
COPY golem.toml.example /golem/golem.toml
RUN sed -i \
      -e 's|^LOCAL_AI_HOST = "127.0.0.1"|LOCAL_AI_HOST = "0.0.0.0"|' \
      -e "s|^GPU_BACKEND = \"auto\"|GPU_BACKEND = \"$GPU_BACKEND\"|" \
      /golem/golem.toml \
    && grep -q '^MODELS_PRESET' /golem/golem.toml \
    && grep -q "^GPU_BACKEND = \"$GPU_BACKEND\"" /golem/golem.toml

# Bake the pinned engine in: a container start must not be a download. This
# is exactly `golem install`'s job - the release the adapter names for this
# platform and backend, into .local/engines/, BUILD marker written last.
# The shared libraries are linked where the loader finds them whatever
# rpath the release carries, and the dry run proves the plan resolves.
RUN golem install \
    && find /golem/.local/engines -name '*.so*' -exec ln -sf {} /usr/local/lib \; \
    && ldconfig \
    && golem up --dry-run

# What belongs to the user, not the image: the model files, the results
# database, the logs and the tuner's overlays. .local (the engine) is baked
# and stays with the image - the image is the engine's home.
VOLUME ["/golem/data/models", "/golem/results", "/golem/logs", "/golem/data/tuned"]

EXPOSE 8000

# PID 1: the service owns the engine and stays alive, starts it on the way
# in (idempotently), stops it on SIGTERM and Ctrl+C, and its stdout is the
# container's log. `docker stop` is the graceful path.
ENTRYPOINT ["golem", "service", "--up"]
