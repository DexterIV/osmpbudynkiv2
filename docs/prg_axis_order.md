# PRG axis order

PRG publishes address points in EPSG:2180. On 2026-10-01 GUGiK changed the
order of the two numbers in every `<gml:pos>`. A reader that assumes the old
order stores every address mirrored, and nothing errors. This is what was
measured, what was built, and what it deliberately does not cover.

Code: `prg_convert` (`common::coord_order_for_srs_name`),
`src/import/prg_axis.rs`, `src/import/prg.rs`. Tests:
`tests/cli_prg_axis_order.rs`.

## What changed upstream

Read straight from the GML, without `prg_convert`:

| snapshot | address points | `srsName` on `<gml:Point>` | order in `<gml:pos>` | zip entries |
|---|---|---|---|---|
| 2026-08-10 | 8,607,258 | `EPSG:2180` | easting northing | `NOWE_*.gml` + 2012-schema `*.xml` |
| 2026-08-14 (= 08-15, same bytes) | 8,608,828 | `EPSG:2180` | easting northing | same |
| 2026-10-01 | 8,624,770 | `urn:ogc:def:crs:EPSG::2180` | northing easting | 16 `.gml` only, renamed |

All 16 voivodeships changed together; no file mixes orders. The first
lubelskie point is `815958 345698` in August and `345698 815958` in October.

The October file is not corrupt. Northing-easting is EPSG:2180's official axis
order, and the URN form of `srsName` is the one that promises it. The same rule
already held for every layout:

| layout | `srsName` | order |
|---|---|---|
| 2012 schema (`*.xml`) | `urn:ogc:def:crs:EPSG::2180` | northing easting |
| 2021 schema until August 2026 | `EPSG:2180` | easting northing |
| 2021 schema from 2026-10-01 | `urn:ogc:def:crs:EPSG::2180` | northing easting |

`prg_convert` up to v0.7.0 did not read `srsName`; it hardcoded one order per
schema, so it reads the October files mirrored.

## What a mirrored load does

- `update prg` diffs **every row as modified**, because PRG compares geometry.
  The apply rewrites the national table, every cell is enqueued, and the next
  drain reports every address unmatched.
- PRG's report signature includes the geometry, so **every active PRG report
  expires**.

## Layer 1: the parser reads the declared order

`prg_convert` v0.8.0 decides per point from the `srsName` of the enclosing
`<gml:Point>`: the short form `EPSG:2180` is easting first; the URN form, any
other form, and a missing attribute are northing first. Both parsers and the
CLI take an override.

Result on the full national snapshots, each converted with v0.8.0 in `auto`:

| | 2026-08-14 | 2026-10-01 |
|---|---|---|
| rows | 8,608,828 | 8,624,770 |
| observed extent | 14.129–24.131°E, 49.063–54.835°N | identical |
| rows outside `POLAND_EXTENT` | 0 | 0 |

Of the 8,595,749 ids present in both, **8,589,935 have a bit-identical
position** and 5,814 moved. That matches the raw files exactly: 8,589,935 ids
carry the same two numbers in reversed order, and 5,814 are ordinary edits.

```sql
WITH a AS (SELECT lokalny_id, dlugosc_geograficzna lon, szerokosc_geograficzna lat
           FROM 'prg_2026-08-14.parquet'),
     o AS (SELECT lokalny_id, dlugosc_geograficzna lon, szerokosc_geograficzna lat
           FROM 'prg_2026-10-01.parquet')
SELECT count(*) AS ids_in_both,
       count(*) FILTER (a.lon = o.lon AND a.lat = o.lat) AS bit_identical,
       count(*) FILTER (a.lon <> o.lon OR a.lat <> o.lat) AS moved
FROM a JOIN o USING (lokalny_id);
```

Through this application, on a scratch database: `import prg` of the August
snapshot (8,608,828 rows, 75 s), then `update prg` to the October one
(8,624,770 rows, 187 s):

| | rows |
|---|---|
| added | 29,021 |
| modified | 49,729 |
| removed | 13,079 |

Read mirrored, `modified` would have been about 8.6 million. The 49,729 is
reproduced exactly by comparing the two converted snapshots on the compared
columns after the `ulica` prefix strip; 5,814 of them are position changes and
the rest are attribute edits. (Aside: 69,900 further rows differ only in the
`ulica`/`ul.` prefix that `ULICA_PREFIX_STRIP_SQL` removes, so they do not
count as modified.)

## Layer 2: the extent check

`import::prg_axis::check_axis_order` runs in `materialize_into`, the one place
`import prg` and `update prg` share, **before** the target table is dropped and
rebuilt. More than `MAX_OUTSIDE_SHARE` (0.1%) of rows outside `POLAND_EXTENT`
(14.0–24.2°E, 49.0–54.9°N) fails the load. `import prg` leaves an existing
`prg_addresses` intact; `update prg` fails before the diff and the live table
keeps serving the previous snapshot.

Why a box for the whole country works, and its limit. Exchanging easting and
northing mirrors a point across the line E = N, which runs through Poland from
Wrocław to Mazury. Most mirrored points stay inside Poland, but the dataset's
extent moves. On the October snapshot:

| axes | lon min | lon max | lat min | lat max | rows outside the box |
|---|---|---|---|---|---|
| correct | 14.129 | 24.131 | 49.063 | 54.835 | 0 |
| EPSG:2180 numbers exchanged | 13.405 | 22.996 | 49.413 | 55.586 | 316,280 (3.67%) |
| lon and lat exchanged | | | | | all 8,624,770 |

Only four voivodeships send any row out of the box when mirrored:

| voivodeship | rows | mirrored rows outside |
|---|---|---|
| lubelskie | 607,987 | 207,047 (34.1%) |
| podlaskie | 291,107 | 71,029 (24.4%) |
| podkarpackie | 569,183 | 21,007 (3.7%) |
| małopolskie | 864,909 | 17,197 (2.0%) |
| the other twelve | 6,291,584 | 0 |

```sql
CREATE TEMP TABLE m AS
SELECT teryt_wojewodztwo, ST_X(g) AS mlon, ST_Y(g) AS mlat
FROM (SELECT *, ST_Transform(ST_FlipCoordinates(ST_Transform(
                  ST_Point(dlugosc_geograficzna, szerokosc_geograficzna),
                  'EPSG:4326', 'EPSG:2180', always_xy := true)),
                'EPSG:2180', 'EPSG:4326', always_xy := true) AS g
      FROM 'prg_2026-10-01.parquet');
SELECT teryt_wojewodztwo, count(*),
       count(*) FILTER (NOT (mlon BETWEEN 14.0 AND 24.2 AND mlat BETWEEN 49.0 AND 54.9))
FROM m GROUP BY 1 ORDER BY 3 DESC;
```

Cost, on a raw-shaped table of 8,218,034 rows with `threads = 8` and
`preserve_insertion_order = false`: 0.14 s for the passing case. It is one
streaming aggregate with no per-row state and completes under
`memory_limit = '150MB'`. The example ids and the exchanged reading are
computed only on the way to an error.

A load that fails is not repaired. The error says whether the exchanged
reading would fit, and `[prg] coordinate_order` / `--coordinate-order` forces
the order through the parser.

## Why an order is corrected in the parser, never in SQL

PRG compares geometry by value. Reprojecting in SQL uses DuckDB's PROJ; the
stored positions came from `prg_convert`'s `proj4rs`. For the same EPSG:2180
input, over the local 8,218,034-row table:

| compared at | rows that differ |
|---|---|
| full `f64` | 2,762,437 (33.6%) |
| 9 decimals | 18 |
| 8 decimals | 4 |
| 7 decimals (about 1.1 cm) | 0 |

The largest disagreement is 2.4 nm in latitude. A position recomputed in SQL
would therefore make a third of the table diff as modified. (Method: the raw
EPSG:2180 values are not stored, so they were recovered by projecting each
stored point back and rounding to the centimetre; every row is within 0.5 mm of
the centimetre grid.)

Snapping stored coordinates to 7 decimals would hide that, and was rejected:
it changes every stored coordinate once, which expires every active PRG report
unless they are re-stamped in the same step.

## The projection library moves coordinates too

`proj4rs` 0.1.9 with `crs-definitions` 0.3.1 and `proj4rs` 0.1.10 with 0.4.0
project the same EPSG:2180 point to latitudes 9e-10° apart, about 0.1 mm, and
to longitudes that differ in the last bit at most. Confirmed when
`prg_convert`'s lockfile was brought from the first pair to the second: for
`287772.37 456005.14` the latitude went from `51.92977532639213` to
`51.929775327307524`. This application was already on the second pair.

A bump like that makes every PRG row diff as modified and expires every active
PRG report. `import::prg::tests::the_parser_projects_a_known_point_to_known_bits`
pins one fixture point bit for bit so that it fails a test first.

## Not covered

- **A mirrored load of one of the twelve voivodeships that mirror onto Polish
  territory** passes the extent check, as does a partial `--file` load of one.
  That includes the lubuskie-only test fixture;
  `prg_axis::tests::a_mirrored_load_of_lubuskie_alone_passes` pins it.
- **Individual records are not checked, repaired or dropped.** 34 address
  points in gmina Mosina (`3021103`) are written in the opposite order to the
  rest of their file in every snapshot checked, October included. They are
  stored 190–192 km from their gmina, near Działoszyn, and are offered for
  import there. A per-gmina test (more than 30 km from the gmina's median
  point, and within 15 km of it once exchanged) finds exactly these 34 among
  8.2M rows; it was measured and decided against.
- 169 of 8,925 małopolskie settlement records (`AD_Miejscowosc`) are written
  in the opposite order to the rest of their file. Nothing reads settlement
  geometry.
