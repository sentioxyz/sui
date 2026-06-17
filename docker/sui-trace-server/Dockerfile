# Build application
#
# Single-stage builder plus minimal runtime. Layer cache short-circuits when the
# COPY inputs are byte-identical to a previous build.
FROM rust:1.92-bullseye AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang libpq5 libpq-dev \
 && rm -rf /var/lib/apt/lists/*
ARG PROFILE=release
WORKDIR /sui

# COPYs ordered least- to most-frequently-changing for deeper layer cache hits.
COPY rust-toolchain.toml ./
COPY .cargo .cargo
COPY Cargo.toml Cargo.lock ./
COPY external-crates external-crates
COPY sui-execution sui-execution
COPY consensus consensus
COPY crates crates

# GIT_REVISION declared late so SHA changes do not invalidate the COPYs above.
ARG GIT_REVISION
ENV GIT_REVISION=$GIT_REVISION
RUN cargo build --locked --profile ${PROFILE} --bin sui-trace-server \
 && case "${PROFILE}" in \
      release) profile_dir=release ;; \
      dev) profile_dir=debug ;; \
      *) profile_dir="${PROFILE}" ;; \
    esac \
 && cp "target/${profile_dir}/sui-trace-server" /usr/local/bin/sui-trace-server

# Production Image
FROM debian:bullseye-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git libpq5 libpq-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /sui
COPY --from=builder /usr/local/bin/sui-trace-server /usr/local/bin/sui-trace-server

ARG BUILD_DATE
ARG GIT_REVISION
LABEL build-date=$BUILD_DATE
LABEL git-revision=$GIT_REVISION
ENV GIT_REVISION=$GIT_REVISION
EXPOSE 9301
ENTRYPOINT ["sui-trace-server"]
