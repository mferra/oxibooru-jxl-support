ARG ALPINE_VERSION=3.23
ARG RUST_VERSION=1.95.0
ARG CHEF_VERSION=0.1.77
ARG FFMPEG_VERSION=8.1.1
ARG DAV1D_VERSION=1.5.3
############################### FFmpeg Phase ###############################
FROM alpine:${ALPINE_VERSION} AS ffmpeg_builder
ARG FFMPEG_VERSION
ARG DAV1D_VERSION
WORKDIR /build

RUN apk add --no-cache build-base curl git meson nasm ninja pkgconfig tar xz

RUN git clone --depth 1 --branch ${DAV1D_VERSION} https://code.videolan.org/videolan/dav1d.git

# Build dav1d as a static musl archive
WORKDIR /build/dav1d
RUN meson setup build \
    --prefix=/usr/local \
    --default-library=static \
    --buildtype=release \
    -Denable_tools=false \
    -Denable_tests=false \
  && ninja -C build install

WORKDIR /build
RUN curl -L https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz | tar -xJ

WORKDIR /build/ffmpeg-${FFMPEG_VERSION}
ARG TARGET_CPU
ENV PKG_CONFIG_PATH=/usr/local/lib/pkgconfig
RUN ./configure \
    --enable-static \
    --enable-libdav1d \
    --enable-protocol=file,pipe \
    --enable-demuxer=avif,matroska,mov,swf \
    --enable-parser=h264,hevc \
    --enable-decoder=flv1,h264,hevc,libdav1d,vp6,vp6a,vp6f,vp8,vp9 \
    --enable-filter=copy,format,null,scale,thumbnail \
    --enable-encoder=rawvideo \
    --enable-muxer=null,rawvideo \
    --enable-swscale \
    \
    ${TARGET_CPU:+--cpu=$TARGET_CPU} \
    --extra-ldflags="-static" \
    --extra-ldexeflags="-static -no-pie" \
    --pkg-config-flags="--static" \
  && make -j$(nproc) \
  && strip ffmpeg

# Verify ffmpeg has no dynamic dependencies
RUN readelf -d ffmpeg | grep -q "NEEDED" \
  && (echo "ffmpeg has dynamic dependencies!" && readelf -d ffmpeg | grep NEEDED && exit 1) \
  || echo "ffmpeg is fully static"

############################## Planning Phase ##############################
FROM docker.io/lukemathwalker/cargo-chef:${CHEF_VERSION}-rust-${RUST_VERSION}-alpine${ALPINE_VERSION} AS chef
WORKDIR /opt/app

# Create recipe of crate dependencies
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

################################ Build Phase ###############################
FROM chef AS builder
ENV DOCKER_DEPLOYMENT=1

RUN mkdir -p /data

RUN apk add --no-cache \
  build-base gcc g++ musl-dev linux-headers \
  libgcc libstdc++ cmake ninja pkgconf perl binutils

COPY --from=planner /opt/app/recipe.json recipe.json

COPY macros/Cargo.toml macros/Cargo.toml
COPY macros/src macros/src

ARG TARGET_CPU=x86-64-v2
ENV RUSTFLAGS="-C target-cpu=x86-64-v2 -C link-arg=-static-libgcc -C link-arg=-static-libstdc++ -C link-arg=-lgcc -C link-arg=-lgcc_eh -C link-arg=-lstdc++"

RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release

RUN readelf -d /opt/app/target/release/oxibooru_server | grep -q "NEEDED" \
  && (echo "Binary has dynamic dependencies!" && readelf -d /opt/app/target/release/oxibooru_server | grep NEEDED && exit 1) \
  || echo "Binary is fully static"

################################# Run Phase ################################
FROM scratch AS runtime
ARG PUID=1000
ARG PGID=1000
ARG FFMPEG_VERSION
WORKDIR /opt/app
ENV RUST_BACKTRACE=full

COPY --from=builder --chown=${PUID}:${PGID} /data /data

# Copy SSL certs for outbound requests
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Copy binaries with ownership
COPY --from=ffmpeg_builder --chown=${PUID}:${PGID} /build/ffmpeg-${FFMPEG_VERSION}/ffmpeg /opt/app/ffmpeg
COPY --from=builder --chown=${PUID}:${PGID} /opt/app/target/release/oxibooru_server /opt/app/server

# Run it!
USER ${PUID}:${PGID}
ENTRYPOINT ["/opt/app/server"]

ENV SERVER_PORT=6666
EXPOSE ${SERVER_PORT}

VOLUME ["/data/"]

ARG DOCKER_REPO
ARG BUILD_DATE
ARG SOURCE_COMMIT
