# Install Oxibooru

## Prerequisites

This guide assumes that you have Docker (version 19.03 or greater) and the Docker Compose CLI (version 1.27.0 or greater) already installed.

## Installing

1. **Download the source**

    ```sh
    git clone https://github.com/maurohoracio/oxibooru-jxl-support
    cd oxibooru-jxl-support
    ```

2. **Configure the application**

    ```sh
    cp server/config.toml.dist server/config.toml
    edit server/config.toml
    ```
    It's *strongly recommended* to at least change these fields:

    - password_secret
    - content_secret

    Any fields not present will default to their corresponding value in the original `config.toml.dist`, so feel free to remove fields that are uneeded or irrelevant.

3. **Pick a compose file**

    There are two compose files, both self-contained (no `.env` file needed — everything is set
    directly in the file; edit it in place if you want different credentials, ports, etc.):

    - **`docker-compose-2.yml`** — builds the server and client images from source. Use this
      unless you're deploying pre-built images from a registry.
    - **`docker-compose.yml`** — pulls pre-built images instead of building. Points at a
      specific registry namespace by default; edit its `image:` fields to point at wherever
      you've published your own images (see [`server/BUILD.md`](../server/BUILD.md#publishing-images-to-a-registry))
      before using it.

    Data and database storage use named Docker volumes in both files, so there's no host mount
    directory to `chown` beforehand.

4. **Build (or pull) and run it**

    Build from source and start:

    ```sh
    docker compose -f docker-compose-2.yml up -d --build
    ```

    ...or, once you have images published to a registry:

    ```sh
    docker compose pull
    docker compose up -d
    ```

    To view/monitor the application logs:

    ```sh
    docker compose logs -f
    # (CTRL+C to exit)
    ```

    If your changes aren't taking effect in a rebuild, add `--no-cache` to the `--build` /
    `docker compose build` step.

    **Performance tip:** if you're building yourself, add a `build.args` block with
    `TARGET_CPU: native` to the `server` service in whichever compose file you're using
    (neither sets it by default). This targets the Rust compiler at your exact CPU, which can
    measurably speed up image decoding and reverse search — at the cost of a binary that may
    not run on other machines.