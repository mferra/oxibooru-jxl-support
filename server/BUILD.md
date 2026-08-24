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

This is the recommended way to run the full stack (server + client + PostgreSQL). The repo
has two compose files, both self-contained (no `.env` file needed — credentials and ports are
set directly in each file; edit them in place if you want different values):

- **`docker-compose-2.yml`** — builds server and client from source, using named Docker
  volumes for data/postgres storage. Use this for local development and testing.
- **`docker-compose.yml`** — pulls pre-built images instead of building. Points at a specific
  registry namespace by default; edit its `image:` fields to point at your own before using it
  (see [Publishing images to a registry](#publishing-images-to-a-registry) below).

### 1. Configure the application

```sh
cp server/config.toml.dist server/config.toml
# Edit server/config.toml — change password_secret and content_secret at minimum
```

### 2. Build (or pull) and start

```sh
# Build from source and start (local development/testing)
docker compose -f docker-compose-2.yml up -d --build

# ...or, once you have images published to a registry (see below):
docker compose pull
docker compose up -d
```

Monitor logs:

```sh
docker compose logs -f
# Ctrl+C to exit
```

Stop the stack:

```sh
docker compose down          # add -f docker-compose-2.yml if you started it that way
```

---

## Publishing images to a registry

`docker-compose.yml` runs pre-built images rather than building on the deployment host. To
publish your own, from the repository root:

```sh
REGISTRY=docker.io/yourusername   # your own registry namespace

docker build -t $REGISTRY/oxibooru_server:latest ./server
docker build -t $REGISTRY/oxibooru_client:latest ./client

docker login
docker push $REGISTRY/oxibooru_server:latest
docker push $REGISTRY/oxibooru_client:latest
```

Then update `docker-compose.yml`'s `image:` fields to `$REGISTRY/oxibooru_server:latest` and
`$REGISTRY/oxibooru_client:latest`, and on the deployment host: `docker compose pull && docker
compose up -d`.

---

## Exposed Port & Volume

| Item              | Value                                              |
|-------------------|-----------------------------------------------------|
| Default HTTP port | `6666` (server, internal), `8099` (client, both compose files) |
| Data volume       | `/data/` inside the container (named volume in both compose files) |
| Config file       | `/opt/app/config.toml` (bind-mounted from host)      |
