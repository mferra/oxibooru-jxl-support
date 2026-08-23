# Upstream port plan (master → main)

Internal tracking doc for reconciling upstream `oxibooru` fixes into this fork's
`main` branch. Not part of the published docs site.

- Fork point (merge-base): `f39cbfdda11631a99d52e5ea38c4419ecf93449f`
- `master` (upstream mirror): 127 commits ahead of the fork point, not yet ported
- `main` (this fork): 40 own commits (JXL, pHash, CBZ pools, admin maintenance actions, SSRF hardening)

## Strategy

**No full `git merge`/rebase of `master` into `main`.** The custom JXL/pHash/CBZ
work heavily diverges from upstream in `content/decode.rs`, `download.rs`,
`cache.rs`, and `hash.rs` — a full merge would produce a large, risky conflict
resolution in exactly the files that matter most.

Instead: port upstream improvements one at a time, as small standalone commits
on `main`, in priority order below. After each port:
- confirm it compiles and the server test suite / `functional-tests` pass
- if a port needs a schema change, add a new numbered migration (`023_...`);
  never edit an already-applied migration
- update this file's status column

## Backlog (priority order)

| # | Status | Upstream commit(s) | Summary | Files | Notes |
|---|--------|--------------------|---------|-------|-------|
| 1 | **DONE** (2026-08-23) | `4740805f` (+ equivalent gap in `user_token.rs`) | Rank check on non-self user/user-token edits — prevents lower-ranked staff (e.g. moderator) from editing/taking over a higher-ranked account (admin) via `user_edit_any_*` privileges | `server/src/api/user.rs`, `server/src/api/user_token.rs` | Ported to all 4 `user_token.rs` handlers (list/create/update/delete), not just `user.rs`, since the same gap existed in all of them |
| 2 | **DONE** (2026-08-23) | `c655b70c` | Byte-sniffing (`infer` crate) instead of trusting extension/`Content-Type` for uploads | `content/decode.rs`, `content/upload.rs`, `content/download.rs`, `filesystem.rs`, `api/error.rs`, `error.rs` | Verified `infer` 0.22.0 recognizes all 11 of our `MimeType` variants (including JXL) with matching MIME strings before porting. `extract.rs` was intentionally left untouched — `main`'s request-dispatch error handling already diverged from upstream's pre-patch version and isn't part of the upload MIME-trust issue. `ContentTypeMismatch`/old `get_mime_type` removed since bytes are now authoritative. Verified via podman: `cargo check` + `cargo clippy` clean, full test suite (106/106) passing against a real Postgres instance |
| 3 | **DONE** (2026-08-23) | `846b656c` + `d9107f2c` | Replace `EnumTables`/`FieldTable<bool>` field-selection with `Mask<Field>` (bitmask) + `Batcher<F>` | new `resource/field.rs`, 22 files under `api/*.rs`, `resource/*.rs` | Applied the transform to `main`'s own code (not a copy of upstream's files) to preserve every divergence: the rank-check fix (item 1), the `custom_thumbnails_exist` batching optimization in `resource/post.rs` (kept intact, adapted to the new `Batcher`), and JXL/pHash/CBZ-specific logic in `post.rs`/`pool.rs` untouched. `string.rs`'s purely cosmetic `Infallible` import change was skipped as not worth the diff. `create_from_archive` (CBZ import) doesn't use `ResourceParams`/`PostInfo` and was untouched. `api/mod.rs`'s `ResourceParams<F>` uses the same `u64: From<F>` trait-bound style as the final polished `field.rs` (not the earlier `F: Into<u64>` style from the raw 846b656c diff) for consistency. Verified via podman: `cargo check` + `cargo clippy` clean (zero new warnings vs. baseline), full test suite (106/106) passing against a real Postgres instance |
| 4 | **DONE, targeted scope** (2026-08-23) | `81287297`, `2ee0b375`, `de6b69ed`, `e49e3f65`, `698ca3ff` | Accurate GIF/AVIF animation detection + `recompute_post_types` admin task | `content/decode.rs`, `content/cache.rs`, `admin/mod.rs`, `admin/post.rs` | User chose the targeted-fix option over full upstream parity (see chat): upstream's final design removes `PostType::from(MimeType)`/`to_image_format` entirely, but `main` has 5+ call sites (`comic.rs` CBZ page filtering, `admin/post.rs`, `update/post.rs` JXL-conversion guard) that rely on the fast mime-only check without a file handle available. Kept `PostType::from`/`to_image_format` intact; added `decode::detect_post_type(file_path, mime_type)` — checks real content for GIF (`image` crate `GifDecoder`) and AVIF (`FFmpeg`-based frame count, matching upstream's *final* de6b69ed approach, not the earlier FFprobe-based one, so no Dockerfile/FFprobe changes needed) and falls back to `PostType::from(mime_type)` for everything else. Wired into `cache.rs`'s upload-time classification alongside the existing animated-WebP special case, and into the new `recompute_post_types` admin task (mirrors `recompute_indexes`' structure) so already-uploaded posts can be reclassified. Verified via podman: `cargo check` + `cargo clippy` clean (one new doc-lint warning found and fixed), full test suite (106/106) passing |
| 5 | **DONE** (2026-08-23) | `9cc21ee7` | Malformed privilege fields were silently ignored | `config.rs`, `app.rs`, `db.rs` | Confirmed `main` had the gap: `PrivilegeConfig::deserialize` never checked for leftover/unknown keys after matching known `Action`s. Also ported the bundled `run_database_migrations` fix (was always returning a sentinel `RangeInclusive::new(1,0)` and unconditionally running `run_server_migrations` on every startup — now returns `Option<RangeInclusive>` and only runs when migrations were actually pending) and `can_send_mails` switching from `#[serde(default)]` to `#[serde(skip)]` (it's server-computed, not user-settable, and the struct has `deny_unknown_fields`) |
| 5 | **DONE** (2026-08-23) | `e8d13f3f` | Avatar path-traversal guard for non-default username regexes | `config.rs` | Confirmed `main`'s `custom_avatar_url`/`custom_avatar_path` built filesystem paths straight from `username.to_lowercase()` with no traversal-character sanitization. Ported upstream's `TRAVERSAL` `AsciiSet` (encodes `/`, `\`, `.`, `%`, controls) and the double-encoding scheme for the URL variant |
| 5 | **DONE** (2026-08-23) | `b9453912` | Lowered max image allocation (4GB→256MB) + added 16384×16384 width/height caps | `content/decode.rs` | Confirmed `main` still had the old 4GB `max_alloc` with no dimension caps — a DoS vector via oversized/malicious image uploads. Applies to the `image` crate's `GifDecoder`/`WebPDecoder`/`ImageReader` paths (not the separate `jxl-oxide`-based JXL decoder, which doesn't use these `Limits`) |
| 5 | **DONE** (2026-08-23) | `1fadaee0` | File-move errors were silently swallowed | `filesystem.rs` | Confirmed real bug: `move_file` pattern-matched only `ErrorKind::CrossesDevices` on the mapped error, so every other rename failure (permission denied, disk full, etc.) fell through silently and execution continued as if the move succeeded. Fixing this surfaced a **pre-existing latent bug in the test fixture setup**: `swap_posts` assumes generated thumbnails always exist on disk (true in production), but `test.rs`'s `create_posts` never created one, so `api::post::test::merge` started failing once the real error was no longer swallowed. Ported upstream's matching test-fixture fix (copy the source media to `generated_thumbnail_path` too) — this is exactly why upstream bundled that same change into this commit |
| — | no action needed | `571790bb` (upstream) vs `5f960a05`/`ec16b4a2`/`dc24d882` (ours) | SSRF hardening on `content/download.rs` | `content/download.rs` | Both sides independently hardened this file; ours looks at least as thorough. Flagged only as a merge-conflict hotspot below, not a missing fix |

## Item #6 — missing view-privilege checks (IN PROGRESS, split into sub-items below)

**Upstream commit:** `e8d6c4a5` ("Fixed multiple issues with privilege handling").
**Confirmed against `main` directly** (not just by the research fork): `api/comment.rs`'s `list` only
checks `Action::CommentList`, and `update`/`rate` check no view privilege at all. **Bug:** if an admin
restricts `*_view` privilege to hide a resource type from some rank, that rank can still **list** it, or
**read it back** by sending an edit/rate/etc. request with an empty/no-op body and inspecting the
response. Same pattern expected in every other resource file. Also bundles two smaller, unrelated
privilege bugs: content downloadable without `upload_use_downloader` privilege, and post `source`
editable with only `post_score` privilege.

**The actual code diff is small per file** (741 lines total across 15 files) — the commit's 345-file /
13,817-line stat is 99% new upstream test *fixture* files (`test/request/**/*.toml|json`) in a snapshot-test
format `main` does not use (`main`'s tests are inline `#[tokio::test]` + `verify_response(query, "fixture/name")`
against `main`'s own fixture files, a different, incompatible layout) — **do not try to port those fixture
files directly**; new coverage for `main` means writing new `#[tokio::test]` cases in each file's existing
`mod test` block, following its established style.

**Pattern to apply everywhere** (confirmed in `main`'s `app.rs:70,75` — already present, no new
infrastructure needed):
- Every `list`/`get`/similar read handler: keep its existing narrower check (e.g. `Action::CommentList`),
  and additionally the resource's `*View` action if not already implied.
- Every `update`/`rate`/`favorite`/mutation-type handler that can leak resource state via its response
  (i.e. returns the resource, even on a no-op edit): add `ctx.verify_privilege(Action::<Resource>View)?`
  near the top, alongside its existing edit-privilege checks.
- Content download / avatar download: gate on `Action::UploadUseDownloader`-equivalent (check `main`'s
  `Action` enum for the exact name) in `content/download.rs` / `content/mod.rs`.
- Post `source` field edit: currently gated only by `post_score`-adjacent privilege in `main`'s
  `update_impl` — needs its own edit-source privilege check (verify `main`'s `Action` enum already has
  a `PostEditSource`-equivalent variant, or whether this needs adding — check before assuming).

**⚠️ Wrinkle for `user.rs`/`user_token.rs` specifically:** upstream's `e8d6c4a5` also deletes the shared
`api::verify_privilege(client, required_rank: UserRank)` free function from `api/mod.rs` (the one our
own item #1 fix uses for the target-rank check) and replaces it with a **private** `fn verify_rank(...)`
defined inside `user.rs` alone. Do **NOT** delete `main`'s `api::verify_privilege` when porting this —
`user_token.rs` also depends on it (from item #1). Recommend keeping it as the shared free function in
`api/mod.rs` under its current name rather than following upstream's later private-per-file refactor;
that's a cosmetic rename we don't need, and duplicating the helper into both files would be worse.
(Note: upstream itself later renames `verify_privilege`→`verify_rank` again in a further commit not yet
investigated — irrelevant to us either way since we're not chasing upstream's exact naming.)

**Sub-items — each independently committable, do in this order (foundational → simple → most-diverged):**

| Sub-item | Status | Files | Approx. code diff (upstream) | Notes |
|---|---|---|---|---|
| 6a | TODO | `api/info.rs`, `api/middleware.rs`, `api/password_reset.rs` | 14 + 3 + 3 lines | Smallest, foundational — `info.rs` gates `featured_post` on `ctx.has_privilege(Action::PostViewFeatured)` (method already exists in `main`). `middleware.rs`/`password_reset.rs` changes are test-only style (`Ok(...).await` vs `...await?; Ok(())`) — skip those, not a behavior fix |
| 6b | TODO | `api/comment.rs` | 109 lines | Confirmed gap in `main` directly (see above). Do this one first among resource files — smallest/simplest resource, good template for the rest |
| 6c | TODO | `api/pool.rs`, `api/pool_category.rs` | 28 + 33 lines | `main`'s `pool.rs` also has the independent CBZ-import endpoint (`create_from_archive`) — confirm it doesn't need a view-privilege check too (it creates, not reads, so probably not, but verify) |
| 6d | TODO | `api/tag.rs`, `api/tag_category.rs` | 67 + 64 lines | Check `get_siblings` specifically — it returns tag info, likely needs the view check too |
| 6e | TODO | `api/user.rs`, `api/user_token.rs` | 147 + 86 lines | **Highest care** — must be reconciled with item #1's rank-check fix in the same functions (see wrinkle above). Do this after 6b-6d once the pattern is well-practiced |
| 6f | TODO | `api/post.rs`, `api/snapshot.rs` | 103 + 15 lines | `post.rs` is `main`'s most diverged file (JXL/pHash/CBZ admin handlers added in item #4: `recompute_phash`, `regenerate_thumbnail`, `convert_to_jxl` — check whether these need a view-privilege check too, upstream has no equivalent to compare against since it doesn't have these handlers). Also apply the two smaller fixes here: post `source` edit privilege, and content-download privilege (may live in `upload.rs`/`content/download.rs` instead, see 6g) |
| 6g | TODO | `api/upload.rs`, `content/download.rs`, `content/mod.rs` | 26 + 9 + 19 lines | The `upload_use_downloader` privilege fix lives here |

**After each sub-item:** run `cargo check` + `cargo clippy` + full test suite in an ephemeral podman
container (see earlier items for the exact pattern — pull `rust:1.95-bookworm`, install
`build-essential cmake nasm pkg-config perl git postgresql`, set up a local `oxi_test` role/db, write
`/.env`), add a few new `#[tokio::test]` cases per file confirming an unauthorized (view-privilege-less)
client is rejected on list/edit, then commit that sub-item alone before moving to the next. Update this
table's Status column as you go so a future session can resume from exactly where this one stopped.

## Known merge-conflict hotspots

Files touched by both histories since the fork point — expect conflicts if any
future rebase/merge is attempted, review carefully when porting individual
fixes that touch these:

`content/decode.rs`, `content/download.rs`, `content/cache.rs`, `content/hash.rs`
(heaviest — JXL/pHash core), `api/post.rs`, `admin/post.rs`, `admin/mod.rs`,
`model/enums.rs`, `config.rs`, `search/post.rs`, `resource/post.rs`,
`update/post.rs`, `Cargo.toml`/`Cargo.lock`, `Dockerfile`, `docker-compose.yml`.
