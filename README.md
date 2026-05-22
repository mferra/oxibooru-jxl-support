This fork is a experimental and personal use, I really not expert on rust, and this was done using CloudeCode, so the presence of errors are likely on the code. I'm still experimenting with this version if JXL runs ok. All credit of creation are for liamw1 on liamw1/oxibooru

# Oxibooru

Oxibooru is an image board engine based on [Szurubooru](https://github.com/rr-/szurubooru). The backend has been entirely rewritten in Rust with a focus on performance 🚀.

If you're interested in migrating a Szurubooru instance to an Oxibooru one, see the [conversion guide](docs/CONVERSION.md). 

If you're interested in contributing, see the [development guide](docs/DEV.md).

## Features

- Post content: images (JPG, PNG, BMP, GIF, WEBP, **JXL**) and videos (MP4, MOV, WEBM), Flash animations
- Native JPEG XL encode/decode with JXL thumbnail support
- Post comments
- Post descriptions
- Post notes / annotations, including arbitrary polygons
- Rich JSON REST API ([see documentation](docs/API.md))
- Token based authentication for clients
- Rich search system with `pool-count` filter and sort token
- Rich privilege system
- Autocomplete in search and while editing tags
- Tag categories
- Tag suggestions
- Tag implications (adding a tag automatically adds another)
- Tag aliases
- Pools and pool categories
- Duplicate and similarity detection
- Post rating and favoriting; comment rating
- Polished UI
- Browser configurable endless paging
- Browser configurable backdrop grid for transparent images

## Installation

It is recommended that you use Docker for deployment. See [installation instructions.](docs/INSTALL.md)

## Building

### Local (native Rust)

Requires the [Rust toolchain](https://www.rust-lang.org/tools/install) and build essentials:

```sh
# Debian/Ubuntu
sudo apt install -y build-essential perl
```

From the `server/` directory:

```sh
cargo build --release
```

The binary is written to `server/target/release/oxibooru_server`.

### Docker image

From the repository root (build context is `server/`):

```sh
docker build -t oxibooru/server:latest ./server
```

Optional build arguments:

| Argument         | Default | Description                                              |
|------------------|---------|----------------------------------------------------------|
| `TARGET_CPU`     | *(auto)*| Set to `native` for CPU-specific optimizations           |
| `RUST_VERSION`   | `1.94.0`| Rust compiler version                                    |
| `FFMPEG_VERSION` | `8.1`   | FFmpeg source version (built statically)                 |
| `DAV1D_VERSION`  | `1.5.3` | dav1d AV1 decoder version (built statically)             |
| `PUID` / `PGID`  | `1000`  | UID/GID the container process runs as                    |

### Docker Compose (full stack)

```sh
# 1. Copy and edit application config
cp server/config.toml.dist server/config.toml

# 2. Copy and edit environment variables
cp example.env .env

# 3. Set mount directory ownership (container runs as UID 1000)
sudo chown -R 1000:1000 "$MOUNT_DATA"
sudo chown -R 1000:1000 "$MOUNT_SQL"

# 4. Build from source (or use `docker compose pull` for pre-built images)
docker compose build

# 5. Start
docker compose up -d
docker compose logs -f
```

See [`server/BUILD.md`](server/BUILD.md) for the full build reference.

## JPEG XL (JXL) Support

This fork adds native JPEG XL support via [`jxl-oxide`](https://crates.io/crates/jxl-oxide) (decode) and [`jpegxl-rs`](https://crates.io/crates/jpegxl-rs) (encode).

### Uploading JXL images

JXL files are accepted as first-class post content. Upload them through the web UI or API exactly like any other image — the server detects the `image/jxl` MIME type automatically and stores the file as-is.

When browsing, the client serves the raw `.jxl` file directly. **The browser must support JXL** to display it. JXL is natively supported in:

- Chrome/Chromium 91+ (flag `chrome://flags/#enable-jxl`) or 122+ (enabled by default in some builds)
- Firefox 90+ with `image.jxl.enabled` set to `true` in `about:config`
- Safari 17+

No server-side transcoding happens at serve time — if the browser does not support JXL the image will not render.

### JXL thumbnails

Thumbnails can be generated in JXL format instead of the default JPEG. Set the following in `server/config.toml`:

```toml
[thumbnails]
avatar_width  = 300
avatar_height = 300
post_width    = 300
post_height   = 300
format = "jxl"   # default is "jpeg"
```

Thumbnail files will have a `.jxl` extension and be served with `image/jxl` content type. As above, the browser must support JXL.

### Bulk conversion: `convert_posts_to_jxl`

The admin CLI includes a task to re-encode all existing image posts to JXL in place. This is useful for reducing storage on large libraries.

**What it does for each eligible post:**
1. Decodes the existing content file (JPG, PNG, WEBP, BMP, AVIF, …)
2. Encodes to JXL using `jpegxl-rs`
3. Writes the new `.jxl` file, updates `mime_type`, `checksum`, and `file_size` in the database
4. Deletes the old content file
5. Regenerates the thumbnail in the currently configured format

Animated GIFs, videos, Flash, and posts already stored as JXL are skipped automatically.

**Running the task:**

Start the server in admin mode:

```sh
# Local binary
./target/release/oxibooru_server --admin

# Docker Compose
docker compose run --rm server --admin
```

At the prompt, select the task:

```
Please select a task: convert_posts_to_jxl
Select posts (leave blank to select all, enter "done" when finished):
```

Leave the post selection blank to convert every eligible post, or enter a search query (same syntax as the web UI) to restrict the conversion to a subset — for example `type:image -type:animated` or `id:100..200`.

Press `Ctrl+C` at any time to cancel; already-converted posts will not be rolled back.

**Other admin tasks** (for reference):

| Task                  | Description                                      |
|-----------------------|--------------------------------------------------|
| `check_integrity`     | Verify post file checksums against the database  |
| `recompute_checksums` | Recompute all post checksums                     |
| `recompute_signatures`| Rebuild reverse-search signatures                |
| `recompute_index`     | Rebuild reverse-search index only (faster)       |
| `regenerate_thumbnails` | Regenerate all post thumbnails                 |
| `reset_passwords`     | Reset user passwords                             |
| `reset_filenames`     | Rebuild the data directory layout                |
| `reset_statistics`    | Rebuild table statistics                         |
| `reset_thumbnail_sizes` | Re-cache thumbnail dimensions                  |
| `convert_posts_to_jxl`| Re-encode image posts as JXL (this fork only)   |

## License

[GPLv3](LICENSE.md).
