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
| 2 | TODO | `c655b70c` | Byte-sniffing (`infer` crate) instead of trusting extension/`Content-Type` for uploads | `content/decode.rs`, `upload.rs`, `filesystem.rs`, `extract.rs` | **Verify first**: confirm `infer` recognizes JXL magic bytes, or add a JXL-specific fallback — JXL is our own addition upstream never had, don't let this start rejecting legit JXL uploads |
| 3 | TODO | `846b656c` + `d9107f2c` | Replace `EnumTables`/`FieldTable<bool>` field-selection with `Mask<Field>` (bitmask) + `Batcher<F>` | new `resource/field.rs`, ~24 files under `api/*.rs`, `resource/*.rs` | Mechanical, low external-compat risk (internal only, doesn't change JSON shape) — but touches files `main` also edits heavily. **Must re-apply our own `13acac9c`** (batched `custom_thumbnails_exist` check) on top — upstream hasn't adopted that optimization yet, don't lose it during the port |
| 4 | TODO | `81287297`, `2ee0b375`, `de6b69ed`, `e49e3f65`, `698ca3ff` | Make `detect_post_type` the single source of truth for post typing; drop FFprobe for animation detection; add `recompute_post_types` admin task | `content/decode.rs` | Needs reconciliation with our own independently-added animated-WebP detection (`12eb7e58`) in the same function — functional overlap, not just a text conflict. Test against JXL and CBZ pools before shipping |
| 5 | needs verification | `9cc21ee7` | Malformed privilege fields were silently ignored | `config.rs`/`app.rs`/`db.rs` | Not yet confirmed whether `main` independently has/lacks this bug |
| 5 | needs verification | `e8d13f3f` | Avatar path-traversal guard for non-default username regexes | `config.rs` | Not yet confirmed against `main` |
| 5 | needs verification | `b9453912` | Lowered max image allocation + width/height caps | `content/decode.rs` | Same file as our JXL decoder — check for conflicts/adjust caps for JXL if ported |
| 5 | needs verification | `1fadaee0` | File-move errors were silently swallowed | `filesystem.rs` | Not yet confirmed against `main` |
| — | no action needed | `571790bb` (upstream) vs `5f960a05`/`ec16b4a2`/`dc24d882` (ours) | SSRF hardening on `content/download.rs` | `content/download.rs` | Both sides independently hardened this file; ours looks at least as thorough. Flagged only as a merge-conflict hotspot below, not a missing fix |

## Known merge-conflict hotspots

Files touched by both histories since the fork point — expect conflicts if any
future rebase/merge is attempted, review carefully when porting individual
fixes that touch these:

`content/decode.rs`, `content/download.rs`, `content/cache.rs`, `content/hash.rs`
(heaviest — JXL/pHash core), `api/post.rs`, `admin/post.rs`, `admin/mod.rs`,
`model/enums.rs`, `config.rs`, `search/post.rs`, `resource/post.rs`,
`update/post.rs`, `Cargo.toml`/`Cargo.lock`, `Dockerfile`, `docker-compose.yml`.
