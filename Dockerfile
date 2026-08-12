# Fully static musl build, shipped in a scratch image.
#
# Nothing links against the host: TLS will be rustls, zlib is zlib-rs (pure
# Rust, and also the only flate2 backend exposing the window-bits control
# permessage-deflate needs), and there is no libpq or OpenSSL anywhere in the
# tree. The C that remains is vendored inside crates (mimalloc), which is why
# musl-dev is installed.

FROM rust:1.95-alpine AS builder

# musl-dev provides the C toolchain mimalloc's build script needs.
RUN apk add --no-cache musl-dev

# The rust:alpine image ships RUSTFLAGS=-Ctarget-feature=-crt-static, which
# disables the static linking musl targets would otherwise default to. An
# environment RUSTFLAGS also overrides .cargo/config.toml, so override it here
# rather than relying on the config file.
ENV RUSTFLAGS="-C target-feature=+crt-static"

WORKDIR /src

# Copy manifests first so dependency compilation caches across source edits.
COPY Cargo.toml Cargo.lock* ./
COPY crates/pahoa/Cargo.toml crates/pahoa/
COPY crates/pahoa-pickle/Cargo.toml crates/pahoa-pickle/
COPY crates/pahoa-pyrandom/Cargo.toml crates/pahoa-pyrandom/
RUN mkdir -p crates/pahoa/src crates/pahoa-pickle/src crates/pahoa-pyrandom/src \
    && echo 'fn main() {}' > crates/pahoa/src/main.rs \
    && touch crates/pahoa-pickle/src/lib.rs crates/pahoa-pyrandom/src/lib.rs \
    && cargo build --release --target x86_64-unknown-linux-musl \
    && rm -rf crates/*/src

COPY . .
# Cargo skips rebuilding when only mtimes moved, so make the real sources newer
# than the placeholder artifacts.
RUN touch crates/*/src/*.rs \
    && cargo build --release --target x86_64-unknown-linux-musl --locked

# Verification is its own stage so `--target builder` stays inspectable when
# something here fails.
#
# Both checks earn their place. The selftest matters as much as the linkage
# check — "it linked" is not the same as "it computes the right answers", and a
# scratch image has no test runner. The linkage check looks for an INTERP
# segment rather than parsing ldd output: Rust emits a static-PIE for musl, and
# musl's own ldd prints a loader line for those even though they are static, so
# ldd cannot distinguish the two.
FROM builder AS verify
RUN set -eux; \
    bin=target/x86_64-unknown-linux-musl/release/pahoa; \
    "$bin" selftest; \
    if readelf -l "$bin" | grep -q INTERP; then \
        echo "ERROR: binary has a program interpreter, so it is not static:"; \
        readelf -l "$bin" | grep -A1 INTERP; exit 1; \
    fi; \
    if readelf -d "$bin" 2>/dev/null | grep -q NEEDED; then \
        echo "ERROR: binary has shared library dependencies:"; \
        readelf -d "$bin" | grep NEEDED; exit 1; \
    fi; \
    echo "static: ok"

FROM scratch
COPY --from=verify /src/target/x86_64-unknown-linux-musl/release/pahoa /pahoa
ENTRYPOINT ["/pahoa"]
