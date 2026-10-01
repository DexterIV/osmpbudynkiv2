# PRG axis order: read it right, and check it on import/update — Implementation Plan

**Goal:** Never store PRG addresses whose X and Y were exchanged.

1. The parser reads each point in the order its `srsName` declares, so both
   the layout GUGiK published until August 2026 and the one it publishes since
   2026-10-01 load correctly with no setting.
2. The load then checks the result against Poland's extent. A snapshot that
   does not fit fails the load and leaves the live table untouched, with a
   message naming the setting that forces the order.

Individual records are out of scope: nothing checks, repairs or drops a single
row whose axes are swapped.

**Status:** implemented on 2026-10-01. `prg_convert` v0.8.0 is tagged and
this repository depends on that tag; the changes here are not committed yet.
Results and final measurements are in `docs/prg_axis_order.md`, which
supersedes the provisional figures below (measured on a local table truncated
in voivodeship `06`): on the full October snapshot a mirrored load puts
316,280 rows (3.67%) outside the box, and twelve voivodeships, not ten, mirror
entirely onto Polish territory.

Left to do: commit, release and deploy before re-enabling
`[jobs.prg_update]`.

**Before anything else:** a deployment still on `prg_convert` v0.7.0 will store
the 2026-10-01 snapshot mirrored on its next `prg_update` run. Disable
`[jobs.prg_update]` there until this ships.

## What changed on 2026-10-01 (verified)

GUGiK's 2021-schema export changed how it writes points. Read straight from
the GML, without `prg_convert`:

| snapshot | address points | `srsName` on `<gml:Point>` | order in `<gml:pos>` | zip entries |
|---|---|---|---|---|
| 2026-08-10 | 8,607,258 | `EPSG:2180` | easting northing | `NOWE_*.gml` + 2012 `*.xml` |
| 2026-08-14 (= 08-15, same bytes) | 8,608,828 | `EPSG:2180` | easting northing | same |
| 2026-10-01 | 8,624,770 | `urn:ogc:def:crs:EPSG::2180` | **northing easting** | 16 `.gml` only, renamed (`06_lubelskie_01.10.2026_….gml`) |

- All 16 voivodeships changed together; no file mixes orders.
- Of the 8,595,749 address ids present in both 08-14 and 10-01, 8,589,935 have
  exactly the same two numbers in reversed order and none have them in the
  same order. The other 5,814 are ordinary position edits.
- The first lubelskie point is `815958 345698` in August and `345698 815958`
  in October.
- The October file is not corrupt: northing-easting is EPSG:2180's official
  axis order, and the URN form of `srsName` is the one that promises it.

**The same rule already holds for every layout on hand:**

| layout | `srsName` | order |
|---|---|---|
| 2012 schema (`*.xml`) | `urn:ogc:def:crs:EPSG::2180` | northing easting |
| 2021 schema until August 2026 | `EPSG:2180` | easting northing |
| 2021 schema from 2026-10-01 | `urn:ogc:def:crs:EPSG::2180` | northing easting |

`prg_convert` v0.7.0 does not read `srsName`. It hardcodes `CoordOrder::YX` for
the 2012 parser (`model2012.rs:390`) and `CoordOrder::XY` for the 2021 parser
(`model2021.rs:483`), which is this rule frozen at the layouts of its day. It
reads the October file mirrored.

Both repositories' fixtures are in the old 2021 layout (`srsName="EPSG:2180"`,
easting first): `fixtures/prg.zip` and `prg_v2.zip` here,
`fixtures/sample_model2021.xml` in `prg_convert`.

## Why nothing catches a mirrored load today

- `import::prg::materialize_into` builds
  `ST_Point(dlugosc_geograficzna, szerokosc_geograficzna)` and checks only that
  both are non-NULL.
- PRG has `compare_geometry: true`, so on `update prg` a mirrored snapshot
  diffs as **every row modified**: the apply rewrites the national table with
  mirrored points, enqueues every cell, and the next drain reports every
  address unmatched. PRG's report signature includes the geometry, so every
  active PRG report expires too. Nothing errors at any step.

## What a mirrored load looks like (measured)

Measured on the local `prg_addresses` (8,218,034 rows; voivodeship `06` is
truncated to 200,000 rows there, so Task 0 repeats this on the full snapshots
in `example_data/PRG/`).

Exchanging easting and northing mirrors a point across the line E = N, which
runs through Poland from Wrocław to Mazury. Most mirrored points stay inside
Poland, but the **extent of the whole dataset moves**:

| axes | lon min | lon max | lat min | lat max |
|---|---|---|---|---|
| correct | 14.129 | 24.046 | 49.063 | 54.835 |
| EPSG:2180 X and Y exchanged | 13.405 | 22.996 | 49.413 | 55.537 |

Against the box 14.0–24.2°E, 49.0–54.9°N:

| | rows outside the box |
|---|---|
| real data | 0 |
| EPSG:2180 X and Y exchanged | about 190,000 (2.3%) |
| lon and lat exchanged (`ST_Point` arguments reversed) | all 8,218,034 |

The rows that leave the box come from the east and south only: podlaskie
40.9% of its rows, lubelskie 49.3%, podkarpackie 5.5%, małopolskie 3.8%, plus
604 rows elsewhere. The other ten voivodeships mirror entirely onto Polish
territory.

## Why positions must come from the parser (measured)

PRG compares geometry by value, so a position that differs from the stored one
in its last bit makes the row diff as modified. The stored positions came from
`prg_convert`'s `proj4rs`; reprojecting in SQL would use DuckDB's PROJ.
Comparing the two over the whole table, for the same EPSG:2180 input:

| compared at | rows that differ, of 8,218,034 |
|---|---|
| full `f64` | 2,762,437 (33.6%) |
| 9 decimals | 18 |
| 8 decimals | 4 |
| 7 decimals (about 1.1 cm) | 0 |

The largest disagreement is 2.4 nm in latitude. So every correction of the
axis order happens in the parser, never in SQL, and the October snapshot read
in the right order lands on exactly the positions the August one produced.

The same sensitivity applies to the projection library. `prg_convert`'s own
lockfile is at `proj4rs` 0.1.9 (with `crs-definitions` 0.3.1), this
application resolves 0.1.10 (0.4.0). `prg_convert`'s tests expect a latitude
0.1 mm from DuckDB's for the same point, while 0.1.10 agrees with DuckDB to
nanometres; that the two versions differ by 0.1 mm is inferred from those two
facts, and Task 1 confirms it. A bump of that size moves every PRG coordinate.

## Design

### Layer 1 — `prg_convert` reads the declared order

- Both parsers take `coordinate_order: Option<CoordOrder>`. `Some` forces an
  order for every point. `None` decides per point from the enclosing
  `<gml:Point srsName="…">`:
  - `urn:ogc:def:crs:EPSG::2180` → northing first (`YX`);
  - `EPSG:2180` → easting first (`XY`);
  - missing or anything else → northing first, the official order and what
    current files use.
- This replaces both hardcoded constants with one rule, and it is why the old
  fixtures and the August snapshots keep loading correctly with no setting.
- The CLI gets `--coordinate-order auto|xy|yx`, default `auto`.

Plainly defaulting the 2021 parser to northing first, with no `srsName` rule,
was the alternative. It reads every 2021 file before October mirrored unless
told otherwise, and a single-voivodeship file such as the lubuskie fixture
would then pass layer 2 mirrored (see *Accepted limitations*), so both
repositories' fixtures would have to be regenerated.

### Layer 2 — the dataset's extent: load or fail

- Reference: one constant lon/lat box for Poland, `POLAND_EXTENT`
  (14.0–24.2°E, 49.0–54.9°N; Poland's real extremes are 14.12, 24.15, 49.00
  and 54.84).
- One query over the keyed, coordinate-bearing rows of `raw_table` counts the
  rows outside the box twice: as parsed, and with `x_epsg_2180` /
  `y_epsg_2180` exchanged and reprojected. The second count only shapes the
  error message, so its last bits do not matter.
- More than `MAX_OUTSIDE_SHARE` (0.001) of the rows outside as parsed →
  `bail!` with `AXIS_ORDER_BAIL_MESSAGE`, the counts, the observed extent and
  example ids. When the exchanged reading fits, the message says so and names
  the override. A mirrored national snapshot is at 2.3%, 23× over; real data
  is at 0. The tolerance exists so one stray record cannot fail a load.
- Cost, measured on a raw-shaped table of 8,218,034 rows with production's
  settings (`threads = 8`, `preserve_insertion_order = false`): 0.65 s. It is
  one streaming aggregate with no per-row state: it completes under
  `memory_limit = '150MB'`, and afterwards `duckdb_memory()` shows only
  `BASE_TABLE` (399 MiB of cached, evictable table blocks, which the load
  select reads next anyway). For scale, the load select that follows peaks at
  1.55 GB of process memory.
- **The example ids come from a separate `… WHERE outside LIMIT 20` query, run
  only when the count is non-zero**, as the missing-coordinates count already
  does. Collecting them with `list(lokalny_id) FILTER (…)` inside the
  aggregate would hold every outside id — 190,000 strings for a mirrored
  snapshot, all 8.6M for reversed `ST_Point` arguments — and that is the one
  way this check could cost real memory.
- The override: `[prg] coordinate_order = "auto" | "xy" | "yx"` in the config,
  and `--coordinate-order` on `import prg` / `update prg`, which wins. It is
  passed straight to the parser, so a forced order still yields exact parser
  positions.
- The point expression becomes one constant (`POINT_SQL`) shared by the check
  and the load select, so the check reads the geometry that will be stored and
  also catches reversed `ST_Point` arguments.
- It runs **before** the target is dropped and rebuilt, and on failure the raw
  table is dropped. `import prg` exits non-zero with an existing
  `prg_addresses` intact; `update prg` fails before the diff, the live table
  keeps serving the previous snapshot, and `job_run_log["update:prg"]` carries
  the error.

A file that declares one order and uses the other is a publisher's error and
gets a person's attention instead of a silent second guess: the load fails,
and one config line fixes it. Re-parsing automatically in that case was the
earlier design; it is dropped because layer 1 now handles the real case, and
keeping it would need the parser to report which order it chose.

### Accepted limitations

- **The extent check needs the east or the south of Poland in the load.** A mirrored
  load confined to one of the ten voivodeships that mirror onto Polish
  territory passes, and so does a partial `--file` load of one. That includes
  the lubuskie-only test fixture.
- **Individually swapped records are left as they are.** 34 address points in
  gmina Mosina (`3021103`) are written in the opposite order to the rest of
  their file in every snapshot checked, October included. They are stored
  190 km from where they belong, near Działoszyn, and are offered for import
  there. Detecting them per gmina was measured and works, and was decided
  against.
- 169 of 8,925 małopolskie settlement records (`AD_Miejscowosc`) are written
  in the opposite order to the rest of their file. Nothing here reads
  settlement geometry, so they are out of scope.

### Rejected

- **Checking, repairing or dropping individual rows.** Decided against.
- **Correcting the order in SQL.** 2,762,437 rows would diff as modified.
- **Snapping stored coordinates to a 7-decimal grid.** It makes SQL and parser
  positions agree by measurement, but changes every stored coordinate once,
  which expires every active PRG report unless re-stamped in the same step.
- **A box per voivodeship**, keyed by `left(teryt_gmina, 2)`. It closes the
  first limitation (a mirrored load leaves at least 9.45% of every
  voivodeship outside its own box) at the cost of 16 measured constants and a
  dependency on the TERC code. Same query shape, so it can replace the
  extent check later.
- **Comparing against the live table on update.** Covers `update` only and
  puts a PRG-specific hook into the generic `refresh`.

## Global constraints

- No change to `compare::rule`, `refresh` or `DatasetSpec`; no
  `TILE_FORMAT_VERSION` or `KV_FORMAT_VERSION` bump.
- New code here goes in a sibling module `src/import/prg_axis.rs` (the extent,
  thresholds, SQL builders, bail message); `prg.rs` only calls it.
- Tests assert on `AXIS_ORDER_BAIL_MESSAGE` by constant, never a substring.
- Every constant carries the measurement that chose it in its doc comment.

## Tasks

### Task 0 — measure on the complete snapshots

`example_data/PRG/` holds full national zips from 2026-01-02 to 2026-10-01.

- [ ] Confirm 0 real rows fall outside `POLAND_EXTENT` on at least two of them.
- [ ] Re-run the measurements above on one and confirm the thresholds still
      separate.
- [ ] Write `docs/prg_axis_order.md` with the tables and queries used here and
      in the snapshot comparison (its scratch files are `axis.duckdb`,
      `analyze.sql`, `extract_pos.py`).

### Task 1 — `prg_convert`: read the declared order, expose it, align dependencies

In `/mnt/nvme/git/prg_convert` (v0.7.0, clean apart from untracked notes):

- [ ] `common.rs`: `coord_order_for_srs_name(Option<&str>) -> CoordOrder`
      holding the three-way rule above, and a non-panicking attribute getter
      beside `get_attribute`, which panics on a missing attribute.
- [ ] Both `AddressParser2012` and `AddressParser2021` get a
      `coordinate_order: Option<CoordOrder>` field set by `new`. Each already
      has a `gml:Point` start-tag arm (`model2012.rs:245`,
      `model2021.rs:340`); it records the point's `srsName`, and the `gml:pos`
      arm resolves the order from the field or that `srsName`, replacing the
      literals at `model2012.rs:390` and `model2021.rs:483`.
- [ ] All four `get_address_parser_*` functions in `lib.rs` take the option as
      a new last argument. Callers to update: the four in `main.rs`'s
      `parse_file`, the test helper at `model2021.rs:701`, and seven tests in
      `lib.rs` (`:220`, `:451`, `:683`, `:720`, `:746`, `:772`, `:794`), all
      passing `None`.
- [ ] CLI: `--coordinate-order auto|xy|yx`, a `clap::ValueEnum` shaped like
      `CrsEpsgArg`, default `auto`, carried in `ParsedArgs`. Tests: each value
      parses, an invalid one is rejected (next to
      `test_parse_rejects_invalid_crs_epsg`). README gets the flag.
- [ ] `CoordOrder` gains `Debug` and `PartialEq`.
- [ ] Tests:
  - every existing test passes with `None` and unchanged fixtures, which is
    the proof that `auto` reproduces both old constants;
  - a copy of `sample_model2021.xml` rewritten the October way (URN
    `srsName`, numbers exchanged) parsed with `None` yields **bit-identical**
    lon/lat and EPSG:2180 columns to the original;
  - a point with no `srsName` is read northing first;
  - `Some(XY)` and `Some(YX)` override the declaration in both parsers.
- [ ] `Cargo.toml`: `proj4rs = "0.1.10"`, so no consumer can resolve the
      version that projects 0.1 mm away. Then `cargo update` to bring the
      lockfile to what this application resolves. Direct dependencies that
      differ today: `proj4rs` 0.1.9 → 0.1.10 (bringing `crs-definitions`
      0.3.1 → 0.4.0), `quick-xml` 0.38.3 → 0.38.4, `reqwest` 0.13.1 → 0.13.4,
      `chrono` 0.4.42 → 0.4.44, `anyhow`, `clap`, `serde_json`, `tempfile`,
      `uuid`. `arrow` (58.1.0) and `zip` (6.0.0) already match.
- [ ] Regenerate the exact-equality coordinate expectations the `proj4rs` bump
      moves: the lon/lat arrays in `src/lib.rs` and the five in
      `tests/e2e.rs`. Expect latitudes to move by about 0.1 mm.
      `common.rs`'s `parse_gml_pos` tests compare within 1e-6 and stay.
- [ ] Convert the real `PRG-punkty_adresowe_2026-10-01.zip` with the CLI and
      confirm it parses at all: the zip has new entry names and no 2012 files.
- [ ] `cargo test`, bump to 0.8.0 (the signatures change), `CHANGELOG.md`, tag.

### Task 2 — this application: use it, and the extent check

- [ ] `Cargo.toml`: move the `prg_convert` tag to v0.8.0. `Cargo.lock` must
      still show `proj4rs` 0.1.10 afterwards.
- [ ] Config: `[prg] coordinate_order`, default `"auto"`, documented in
      `example_config.toml` as an override for a file whose declaration is
      wrong. CLI: `--coordinate-order` on `import prg` and `update prg`,
      taking priority over the config the way `--terc-file` does.
- [ ] `prg.rs`: `stream_gml_into` takes the option and passes it to
      `get_address_parser_2021_zip` (`prg.rs:331`, the only call site).
- [ ] `prg_axis.rs`: `POLAND_EXTENT`, `MAX_OUTSIDE_SHARE`,
      `AXIS_ORDER_BAIL_MESSAGE`, `check_axis_order(conn, raw_table) -> Result<()>`.
      `prg.rs`: extract `POINT_SQL`; call the check before the CTAS batch;
      drop `raw_table` on its error.
- [ ] Guard test: one record of `fixtures/prg.zip` streamed through the real
      parser has exactly the expected `dlugosc_geograficzna` /
      `szerokosc_geograficzna` bits. Its doc comment says what a failure
      means: the projection moved, so every PRG row will diff as modified and
      every active PRG report will expire.
- [ ] The existing hand-built fixtures all sit at 21–22°E, 52–53°N and pass
      the check unchanged.
- [ ] Unit tests, each with its negative. The mirrored cases need rows from
      the east (for example around Hrubieszów, 23.9°E 50.8°N):
  - `fixtures/prg.zip` streamed into a raw table passes;
  - eastern rows pass as they are and bail with EPSG:2180 X and Y exchanged,
    with a message saying the exchanged reading fits;
  - rows with lon and lat exchanged bail, with a message saying neither fits;
  - a load at exactly the tolerance passes, one row over bails;
  - the lubuskie fixture passes even when mirrored, pinning the first
    limitation so nobody mistakes the check for stronger than it is.

### Task 3 — end to end

- [ ] Test helper that rewrites `fixtures/prg.zip` into a temp zip: every
      `<gml:Point>` given the URN `srsName` and its numbers exchanged (the
      October layout), or every `<gml:pos>` replaced by an eastern point in a
      chosen order under a chosen `srsName`. The `zip` crate is already a
      dependency; no new binary fixture.
- [ ] `tests/cli_update_prg.rs`: import the fixture, update to its
      October-layout rewrite. The diff is **0 added, 0 modified, 0 removed**.
- [ ] `tests/cli_import_prg.rs`: importing the October-layout rewrite yields
      the same `geom` values as importing the original.
- [ ] A zip of eastern points whose declaration contradicts its numbers:
      import and update both fail with the bail message; the live table, row
      count and a sampled `geom` are unchanged; no staging or raw table
      remains; no `dataset_refreshes` row; `job_run_log["update:prg"]` is
      `Error`. With `--coordinate-order` forcing the right order, the same zip
      loads.
- [ ] On real data, by hand: `import prg --file` the 08-14 zip, then
      `update prg --file` the 10-01 zip. Expect `modified` in the thousands
      (the 5,814 position edits plus attribute edits), not 8.6 million.
- [ ] `cargo test`, `cargo clippy`, `cargo fmt -- --check`.

### Task 4 — document

- [ ] `CLAUDE.md`: a gotcha under *Geometry* covering the `srsName` rule and
      that it lives in `prg_convert`; that the check's signal is the extent
      and comes from the east and south only; that it runs before the target
      is dropped; that an order correction must go through the parser and
      never SQL (the 33.6% of rows that differ in their last bits); that
      individual swapped rows are deliberately not handled; and the `proj4rs`
      guard test.

## Decided

- The parser reads the order each point declares, falling back to northing
  first; both parsers and the CLI take an override.
- A load that does not fit Poland fails; it is not re-parsed automatically.
- Individual rows are not checked, repaired or dropped.
- The extent check has no disable flag. Add one only if a false positive is observed.
