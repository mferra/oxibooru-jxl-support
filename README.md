This fork is a experimental and personal use, I really not expert on rust, and this was done using ClaudeCode, so the presence of errors are likely on the code. I'm still experimenting with this version if JXL runs ok. All credit of creation are for liamw1 on liamw1/oxibooru

# Oxibooru

Oxibooru is an image board engine based on [Szurubooru](https://github.com/rr-/szurubooru). The backend has been entirely rewritten in Rust with a focus on performance 🚀.

If you're interested in migrating a Szurubooru instance to an Oxibooru one, see the [conversion guide](docs/CONVERSION.md).

If you're interested in contributing, see the [development guide](docs/DEV.md).

## Features

- Post content: images (JPG, PNG, BMP, GIF, WEBP, **JXL**) and videos (MP4, MOV, WEBM), Flash animations
- Native JPEG XL encode/decode with JXL thumbnail support
- **Animated WebP detection** — animated WebP files are correctly classified as `type:animation` instead of `type:image`
- **On-upload transcoding** — GIF animations → animated WebP or AV1 MP4 (whichever is smaller); static images → JXL (optional, gated by config)
- **Perceptual hash (pHash)** — DCT-based 64-bit hash for format/resolution-independent duplicate detection; searchable with `similar:POST_ID[,THRESHOLD]`
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

> **Note:** This fork uses the `array_windows` unstable Rust feature (in pre-existing upstream code). A **nightly** toolchain is required.

Requires the [Rust toolchain](https://www.rust-lang.org/tools/install) and build essentials:

```sh
# Debian/Ubuntu
sudo apt install -y build-essential perl
```

From the `server/` directory:

```sh
cargo +nightly build --release
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

---

## JPEG XL (JXL) Support

This fork adds native JPEG XL support via [`jxl-oxide`](https://crates.io/crates/jxl-oxide) (decode) and [`jpegxl-rs`](https://crates.io/crates/jpegxl-rs) (encode).

### Uploading JXL images

JXL files are accepted as first-class post content. Upload them through the web UI or API exactly like any other image — the server detects the `image/jxl` MIME type automatically and stores the file as-is.

When browsing, the client serves the raw `.jxl` file directly. **The browser must support JXL** to display it. JXL is natively supported in:

- Chrome/Chromium 122+ (enabled by default in some builds)
- Firefox 90+ with `image.jxl.enabled` set to `true` in `about:config`
- Safari 17+

No server-side transcoding happens at serve time — if the browser does not support JXL the image will not render.

### JXL thumbnails

Thumbnails can be generated in JXL format instead of the default JPEG. Set the following in `server/config.toml`:

```toml
[thumbnails]
format = "jxl"   # default is "jpeg"
# jxl_quality = 75  # thumbnail JXL quality (0–100, default 75)
```

---

## Animated WebP Detection

Previously, all WebP files were classified as `type:image` regardless of whether they contained animation. This fork correctly identifies animated WebP by scanning for the `ANIM` chunk in the RIFF container.

- Animated WebP → `type:animation`
- Static WebP → `type:image`

This classification happens at upload time and is always active regardless of the transcoding setting. Animated WebP files are also excluded from the `convert_posts_to_jxl` bulk conversion.

---

## On-Upload Transcoding

An optional pipeline can automatically transcode uploaded content to more efficient formats. It is **disabled by default**.

### Configuration

Add to `server/config.toml`:

```toml
[transcoding]
enabled = true

# Quality for static image → JXL conversion (0–100, default 90)
image_quality = 90

# How to transcode GIF animations: "smallest" | "webp" | "av1"
#   smallest — encode both and keep whichever is smaller (default)
#   webp     — always produce animated WebP
#   av1      — always produce AV1 MP4, fall back to WebP if AV1 fails
animation_format = "smallest"
```

### What gets transcoded

| Uploaded format      | Result                                         |
|----------------------|------------------------------------------------|
| GIF animation        | Animated WebP or AV1 MP4 (whichever is smaller)|
| Static image (JPG, PNG, WebP, BMP, AVIF, …) | JXL at `image_quality` |
| JXL                  | Stored as-is (no re-encode)                    |
| Animated WebP        | Stored as-is (already optimal)                 |
| Video (MP4, WEBM, …) | Stored as-is                                   |
| Flash (SWF)          | Stored as-is                                   |

### AV1 probe

At startup (when transcoding is enabled) the server probes the bundled FFmpeg binary for `libaom-av1` support. If the encoder is absent it logs a warning and falls back to WebP for GIF transcoding; no manual configuration is needed.

### Checksums

Checksums (`checksum`, `checksum_md5`) are always computed from the **stored** (possibly transcoded) file, so `check_integrity` and duplicate detection work correctly regardless of what format was originally uploaded.

---

## Perceptual Hash (pHash)

Every post now carries a 64-bit DCT-based perceptual hash. Similar-looking images share a low [Hamming distance](https://en.wikipedia.org/wiki/Hamming_distance) between their hashes, regardless of format, resolution, or minor edits.

### How it works

1. The uploaded image is resized to 32×32 grayscale.
2. A 2D DCT is applied and the top-left 8×8 coefficient block is extracted (64 values).
3. Each of the 64 bits is set to 1 if its DCT coefficient is above the mean.

The resulting `i64` is stored in the `post.phash` column (nullable; NULL for pre-existing posts until `calculate_phash` is run). There is **no uniqueness constraint** — the same image in different formats will produce the same hash value, which is intentional.

For video and animation posts the hash is computed from the representative frame (same frame used for thumbnails and reverse-search signatures).

### Searching by similarity

Use the `similar:` search token in the post search box or via the API:

```
similar:POST_ID
similar:POST_ID,THRESHOLD
```

- `POST_ID` — the reference post whose pHash is used for comparison.
- `THRESHOLD` — integer from 1 to 100 (percentage similarity, **default 80**).

The threshold maps to a maximum Hamming distance:

```
max_bits = floor((100 - THRESHOLD) × 64 / 100)
```

| Query              | Max differing bits | Meaning                    |
|--------------------|--------------------|----------------------------|
| `similar:42,100`   | 0 bits             | Exact pHash match           |
| `similar:42,85`    | 9 bits             | Near-duplicate              |
| `similar:42,80`    | 12 bits            | Visually similar (default)  |
| `similar:42,50`    | 32 bits            | Loosely related             |

> **Note:** `similar:42` and `similar:42,100` differ — the first uses the default threshold of 80 (≤12 bits), while the second requires an exact 64-bit hash match. Use `,100` to find true duplicates across different formats.

The reference post itself is included in results (its own Hamming distance is 0). Prefix with `-id:POST_ID` to exclude self.

Posts where `phash IS NULL` are always excluded from `similar:` results.

> Compatible with PostgreSQL 9+. The similarity query uses `bit_count(bit(64))` which has been available since PostgreSQL 9.

---

## Admin Commands

Start the server in admin mode:

```sh
# Local binary
./target/release/oxibooru_server --admin

# Docker Compose
docker compose run --rm server --admin
```

At the prompt, type a task name and press Enter. Leave the post selection blank to operate on all posts, or enter a search query to restrict the operation.

| Task                    | Description                                                     |
|-------------------------|-----------------------------------------------------------------|
| `check_integrity`       | Verify post file checksums against the database                 |
| `recompute_checksums`   | Recompute all post checksums                                    |
| `recompute_signatures`  | Rebuild reverse-search signatures                               |
| `recompute_index`       | Rebuild reverse-search index only (faster)                      |
| `regenerate_thumbnails` | Regenerate all post thumbnails                                  |
| `reset_passwords`       | Reset user passwords                                            |
| `reset_filenames`       | Rebuild the data directory layout                               |
| `reset_statistics`      | Rebuild table statistics                                        |
| `reset_thumbnail_sizes` | Re-cache thumbnail dimensions                                   |
| `convert_posts_to_jxl`  | Re-encode static image posts as JXL and regenerate thumbnails   |
| `calculate_phash`       | Compute perceptual hash for posts that don't have one yet       |

### `convert_posts_to_jxl`

Re-encodes all eligible image posts to JXL in-place:

1. Decodes the existing file (JPG, PNG, WEBP, BMP, AVIF, …)
2. Encodes to JXL at the quality set in `[transcoding] image_quality`
3. Writes the new `.jxl` file, updates `mime_type`, `checksum`, and `file_size` in the database
4. Removes the old content file
5. Regenerates the thumbnail

Animated GIFs, animated WebP, videos, Flash, and posts already stored as JXL are skipped automatically.

### `calculate_phash`

Computes and stores the perceptual hash for posts whose `phash` column is NULL (i.e. pre-existing posts uploaded before this fork was deployed). Safe to run multiple times — already-hashed posts are skipped. Runs in parallel across available CPU cores.

```
Please select a task: calculate_phash
Select posts (leave blank to select all, enter "done" when finished):
```

Leave the post selection blank to process every post without a pHash, or enter a search query to restrict — for example `type:image` to process only static images first.

---

## License

[GPLv3](LICENSE.md).
