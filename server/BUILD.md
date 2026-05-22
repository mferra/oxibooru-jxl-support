# Building the Server

## Local Build (Native)

### Prerequisites

- [Rust toolchain](https://www.rust-lang.org/tools/install) (install via `rustup`)
- Build essentials and Perl (required by OpenSSL vendored build):

```sh
# Debian/Ubuntu
sudo apt update && sudo apt install -y build-essential perl

# Fedora/RHEL
sudo dnf install -y gcc make perl
```

### Compile

From the `server/` directory:

```sh
# Debug build (faster compile, slower runtime)
cargo build

# Release build (optimized, ~10-100x faster runtime)
cargo build --release
```

The binary is output to:
- Debug: `server/target/debug/oxibooru_server`
- Release: `server/target/release/oxibooru_server`

### Run Locally

```sh
cargo run
# or with release profile:
cargo run --release
```

The server requires a running PostgreSQL instance and a `config.toml` in the working directory:

```sh
cp config.toml.dist config.toml
# Edit config.toml — at minimum change password_secret and content_secret
```

Set the required environment variables before running:

```sh
export POSTGRES_HOST=localhost
export POSTGRES_USER=oxi
export POSTGRES_PASSWORD=changeme
export POSTGRES_DB=oxi
export POSTGRES_PORT=5432
```

---

## Container Build (Docker)

The Dockerfile performs a fully static multi-stage build:

1. **FFmpeg phase** — builds a static FFmpeg binary with dav1d (AV1) support
2. **Planning phase** — generates a `cargo-chef` recipe for dependency caching
3. **Build phase** — compiles the Rust binary against musl libc (fully static)
4. **Runtime phase** — copies binaries into a `scratch` image (~no OS overhead)

### Build the image

From the **repository root** (the build context must be `server/`):

```sh
docker build -t oxibooru/server:latest ./server
```

#### Optional build arguments

| Argument          | Default    | Description                                                                 |
|-------------------|------------|-----------------------------------------------------------------------------|
| `TARGET_CPU`      | *(auto)*   | Set to `native` for CPU-specific optimizations (less portable binary)       |
| `CODEGEN_OPTIONS` | `-C target-cpu=$TARGET_CPU` | Raw `RUSTFLAGS` passed to `cargo build`            |
| `ALPINE_VERSION`  | `3.23`     | Base Alpine version                                                         |
| `RUST_VERSION`    | `1.94.0`   | Rust compiler version                                                       |
| `FFMPEG_VERSION`  | `8.1`      | FFmpeg source version to build                                              |
| `DAV1D_VERSION`   | `1.5.3`    | dav1d (AV1 decoder) source version to build                                |
| `PUID` / `PGID`   | `1000`     | UID/GID the container process runs as                                       |

Example with `native` CPU optimizations:

```sh
docker build --build-arg TARGET_CPU=native -t oxibooru/server:latest ./server
```

---

## Running with Docker Compose

This is the recommended way to run the full stack (server + client + PostgreSQL).

### 1. Configure the application

```sh
cp server/config.toml.dist server/config.toml
# Edit server/config.toml — change password_secret and content_secret at minimum
```

### 2. Configure environment variables

```sh
cp example.env .env
# Edit .env
```

Key variables in `.env`:

| Variable           | Description                                      |
|--------------------|--------------------------------------------------|
| `POSTGRES_USER`    | PostgreSQL username                              |
| `POSTGRES_PASSWORD`| PostgreSQL password                              |
| `POSTGRES_DB`      | PostgreSQL database name                         |
| `POSTGRES_PORT`    | PostgreSQL port (default `5432`)                 |
| `PORT`             | Host port to expose the web UI (e.g. `8080`)     |
| `MOUNT_DATA`       | Host path for image/media data                   |
| `MOUNT_SQL`        | Host path for database files                     |

### 3. Set mount directory permissions

The container runs as UID/GID 1000. Set ownership before starting:

```sh
sudo chown -R 1000:1000 "$MOUNT_DATA"
sudo chown -R 1000:1000 "$MOUNT_SQL"
```

### 4. Build or pull containers

To build from source:

```sh
docker compose build
```

To pull pre-built images from docker.io:

```sh
docker compose pull
```

### 5. Start the stack

```sh
docker compose up -d
```

Monitor logs:

```sh
docker compose logs -f
# Ctrl+C to exit
```

Stop the stack:

```sh
docker compose down
```

---

## Exposed Port & Volume

| Item              | Value          |
|-------------------|----------------|
| Default HTTP port | `6666` (internal), mapped via `PORT` in `.env` |
| Data volume       | `/data/` inside the container                  |
| Config file       | `/opt/app/config.toml` (bind-mounted from host) |
