# Functional tests

Browser-driven functional tests for the oxibooru client, using
[`fantoccini`](https://docs.rs/fantoccini) (a pure-Rust async WebDriver client)
to drive headless Firefox via `geckodriver` — the same WebDriver protocol
Selenium uses.

## Prerequisites

- `geckodriver` listening on `127.0.0.1:4444` (e.g. `geckodriver --port 4444`).
- A running oxibooru stack (nginx + server + postgres) reachable at the
  `BASE_URL` configured at the top of each test file (default
  `http://127.0.0.1:8088`).
- An administrator account and a regular account matching the credentials
  configured at the top of each test file, and the post(s) referenced by the
  test must exist.

## Running

```sh
cargo run --bin post_maintenance
```

`post_maintenance` exercises the per-post admin maintenance actions
("Recalculate perceptual hash", "Regenerate thumbnail", "Convert to JPEG XL")
on the post edit page, and verifies that the maintenance section is hidden
from non-administrators.
