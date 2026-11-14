FROM --platform=$BUILDPLATFORM ghcr.io/astral-sh/uv:0.12.11@sha256:79c6f4776b851471cc73b7d21d0cc834bb94383c292e83640d27eff512864df7 AS uv

FROM --platform=$BUILDPLATFORM ubuntu:24.04 AS build-tools
ENV HOME="/root"
WORKDIR $HOME

RUN apt-get update && apt-get install -y --no-install-recommends build-essential curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install uv
COPY --from=uv /uv /usr/local/bin/uv

# Setup zig as cross compiling linker
COPY pyproject.toml uv.lock ./
RUN uv sync --only-group docker --locked --no-cache
ENV PATH="$HOME/.venv/bin:$PATH"

FROM build-tools AS build

# The companion uses pinned parser/host sources, without building the checker.
WORKDIR $HOME/app

# Install rust
ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
    "linux/arm64") echo "aarch64-unknown-linux-musl" > rust_target.txt ;; \
    "linux/amd64") echo "x86_64-unknown-linux-musl" > rust_target.txt ;; \
    *) exit 1 ;; \
    esac
# Update rustup whenever we bump the rust version
COPY companion/rust-toolchain.toml rust-toolchain.toml
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --target $(cat rust_target.txt) --profile minimal --default-toolchain none
ENV PATH="$HOME/.cargo/bin:$PATH"
# Installs the correct toolchain version from rust-toolchain.toml and then the musl target
RUN rustup target add $(cat rust_target.txt)

# Build only the companion. Keep path-dependency workspace manifests intact.
ENV CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
COPY companion/Cargo.toml companion/
COPY companion/src companion/src
RUN cargo zigbuild --locked --manifest-path companion/Cargo.toml --bin ty-extended --target $(cat rust_target.txt) --release \
    && cp companion/target/$(cat rust_target.txt)/release/ty-extended /ty-extended \
    && cargo clean --manifest-path companion/Cargo.toml \
    && mkdir -m 1777 /runtime-tmp

# Bundle an unchanged official checker. Its version is independent of the companion.
RUN case "$TARGETPLATFORM" in \
    "linux/arm64") checksum=1aeb90425e934914f437f30ec7de6f5a55352aa858628903adb08a24a852191e ;; \
    "linux/amd64") checksum=cae79385c6db78ecb62ff113edd02d8b79536849beb9771f43a5a68e1df7ca21 ;; \
    *) exit 1 ;; \
    esac \
    && target=$(cat rust_target.txt) \
    && curl --proto '=https' --tlsv1.2 -LsSf "https://github.com/astral-sh/ty/releases/download/0.0.99/ty-${target}.tar.gz" -o /tmp/official-ty.tar.gz \
    && echo "${checksum}  /tmp/official-ty.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/official-ty.tar.gz -C /tmp \
    && cp "/tmp/ty-${target}/ty" /ty \
    && rm -rf /tmp/official-ty.tar.gz "/tmp/ty-${target}"

FROM scratch
COPY --from=build --chmod=1777 /runtime-tmp /tmp
COPY --from=build /ty-extended /ty-extended
COPY --from=build /ty /ty
WORKDIR /io
ENTRYPOINT ["/ty-extended"]
