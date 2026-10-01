# Statistics page

Written 2026-09-27. A separate page, `web/stats.html`, reached from a
"Statystyki" link in the masthead next to "Opcje widoku". It shows the state
of the server's data, headline counts for OSM and the three government
datasets, and a ranking of gminy or settlements by missing PRG addresses.

It is backed by two read-only endpoints: `GET /stats` (everything except the
ranking) and `GET /stats/areas` (the ranking, paginated).

All numbers below come from the local Poland database (DuckDB 1.5.5,
2026-09-27).

## Page contents

Top to bottom: data freshness (1), the public registers (3), reports (from 1),
OSM (2), then the ranking (4). The numbering below follows the original
list, not the page order.

### 1. Stan serwera (server state)

| item | source |
|---|---|
| Last successful load of PRG / BDOT10k / EGIB | the later of `metadata.imported_at:<source>` (written by a successful `import`) and `max(finished_at)` in `dataset_refreshes` (written by a successful `update`, including no-op ones) |
| OSM data up to | `metadata.osm_replication_timestamp`, which only moves when an `import osm` or `update osm` batch succeeds |
| OSM replication lag | the request time minus the replication timestamp, the same computation as `/status` |
| Reports | `object_reports` counted by source × status (`active` / `revoked` / `expired`) |

`imported_at:<source>` was added for this page. Before it existed an import
left no durable timestamp at all (`job_run_log` keeps only the latest run,
failed or not, and `import prg` writes no row). A database imported before
that change has no stamp until its next import, so its time is taken from
refreshes alone.

### 2. OSM

| item | source | local value |
|---|---|---:|
| Addresses: nodes / ways / relations / total | `osm_addresses GROUP BY osm_type` | 3,272,038 / 5,416,880 / 3,665 / 8,692,583 |
| Buildings: ways / relations / total | `osm_buildings GROUP BY osm_type` | 17,989,407 / 4,896 / 17,994,303 |
| Former buildings | `count(*)` on `osm_former_buildings` | 15,407 |
| Top 10 `building` values | `osm_buildings GROUP BY building`, ties broken by value | `yes` 10,780,119, `house` 1,560,094, … |

These are object counts, not deduplicated addresses. Deduplication was
dropped: without TERYT codes in OSM, "the same address" can only be judged on
text.

A former building is an object tagged with a lifecycle-prefixed building key
(`demolished:building`, `was:building`, …) and no live `building` key. That is
exactly the set the suppression veto reads.

### 3. PRG, BDOT10k and EGIB

One row per source:

| column | meaning |
|---|---|
| Porównywane | objects that take part in the comparison |
| Niedopasowane | `count(*)` on `<source>_unmatched` |
| % niedopasowanych | unmatched / comparable, computed by the client |

"Porównywane" is `count(*)` on the source table, with the filter the match
rule applies to it. Only BDOT10k has one, and the count reuses
`rule::BDOT10K_EKSPLOATOWANY_FILTER` rather than restating it.

| Zbiór | Porównywane | Niedopasowane | % |
|---|---:|---:|---:|
| Adresy (PRG) | 8,615,421 | 518,120 | 6.0 % |
| Budynki (BDOT10k) | 16,016,117 | 485,317 | 3.0 % |
| Budynki (EGIB) | 17,589,332 | 1,924,717 | 10.9 % |

No "total" row. Rows the loaders drop, such as those with invalid geometry or
a NULL key, never reach the table, and their counts survive only as free text
in `job_run_log`. So for PRG and EGIB the table count already *is* the
comparable count.

The difference between comparable and unmatched is not broken down into
matched, suppressed and reported. Suppression is expensive to count
(`suppressed_buildings_sql` takes 4.7 s and 3.8 GB per source over the
national extent), and report precedence makes a subtraction double-count.

### 4. Ranking: niedopasowane adresy według obszaru

One table with four controls:

| control | values | default |
|---|---|---|
| Poziom | gmina / miejscowość | gmina |
| Sortuj według | % niedopasowanych / liczba niedopasowanych | % |
| Kolejność | malejąco / rosnąco | malejąco |
| Obszar | one województwo, powiat or gmina, picked from suggestions | the whole country |

**Obszar narrows the ranking to one unit; it is not a text filter.** The field
suggests units by name (ignoring case, diacritics and `ł`) or by TERC code
prefix, and only a picked suggestion takes effect: typed text that was not
picked is dropped when the field loses focus. It is independent of Poziom, so
a picked gmina is one row under "gminy" and its settlements under
"miejscowości". The `#` column is the position within the narrowed ranking.

The suggestions come from `teryt_units` (see `import/teryt.rs`), the TERC
catalogue a PRG load stores: 2,875 units, sent whole to the browser by
`GET /stats/units` and filtered there. Each suggestion shows what tells it
from its namesakes -- the gmina type, powiat and województwo -- and its code.
Settlements are not searchable: there are ~50,000 of them and `teryt_units`
holds only TERC. Finding one means picking its gmina and switching Poziom.

The table is paginated, 50 rows per page. Its columns:

| column | gmina level | settlement level |
|---|---|---|
| # | rank under the current sort | same |
| Gmina / Miejscowość | `gmina` + type (miejska / wiejska / miejsko-wiejska, from the last TERC digit) | `miejscowosc` |
| TERC / SIMC | `teryt_gmina` | `teryt_miejscowosc` |
| Gmina | — | `gmina` + type |
| TERC gminy | — | `teryt_gmina` |
| Adresy | `count(*)` on `prg_addresses` per row | same |
| Niedopasowane | `count(*)` on `prg_unmatched` per row | same |
| % | niedopasowane / adresy | same |
| Mapa | link to the map fitted to the extent of the row's PRG addresses | same |

**A row is a group of `prg_addresses` by code and name together:**
`(teryt_gmina, gmina)` for gminy, and
`(teryt_miejscowosc, miejscowosc, teryt_gmina, gmina)` for settlements.
`prg_unmatched` carries the same four columns, copied at compare time, so it
groups identically and joins on them. No name ever has to be chosen for a
code:

- Every gmina code has exactly one name in PRG, so the gmina level is exactly
  one row per code (2,479).
- At settlement level, the two settlements published under two spellings and
  the one spanning two gminy are two rows each. The 226 addresses with an
  empty SIMC code stay apart as the separate places they are, instead of
  merging into one row. That gives 52,575 rows for 52,560 codes.
- Both levels sum to the national figures: 8,615,421 comparable and 518,120
  unmatched addresses.
- Areas with zero missing addresses are included. They sort to the end under
  the default order.

6,165 settlement names belong to more than one place, and a town and its
surrounding rural gmina often share a name. That is why every row shows its
code, and the gmina type or the gmina.

**Known consequence of the defaults.** At gmina level, a percentage ranking
is meaningful: the smallest gmina has 423 addresses. At settlement level it is
led by hamlets that are 100% missing (the median settlement has 52 addresses,
and 5,944 have fewer than 10). A minimum-size filter is listed under
[Later](#later).

## Serving

Both endpoints are **plain queries run per request**, with no server-side
state. The only caching is HTTP: `http_cache::CacheHeaders` gains
`stats: public, max-age=<stats_max_age_seconds>` (default 300). There is no
`ETag`. A 400 or 500 sets no header, so it inherits the outer `no-store`.

- `/stats` runs its handful of counts in one read transaction, so they all
  describe the same moment. ~0.07 s on the Poland database.
- `/stats/areas` is one aggregation per request: group `prg_addresses` and
  `prg_unmatched` by the level's key columns, `LEFT JOIN`, sort, then
  `LIMIT`/`OFFSET`. `count(*) OVER ()` gives `total_items` in the same pass;
  only a page past the end, which returns no row, runs a separate count.
  ~0.11 s for gminy, ~0.12 s for settlements, whatever the page.
- The sort is the chosen key (`coalesce(unmatched, 0) / total` or
  `coalesce(unmatched, 0)`) in the chosen direction, then the key columns
  ascending. The key columns make the order total, so pages never overlap or
  skip rows. The `ORDER BY` text comes from fixed Rust enums, never from the
  request.
- Pages read live data, so a recompute landing between two page requests can
  move a row from one page to the next. Accepted in exchange for having no
  state.

**The extent uses `ST_Extent_Agg`, which is safe only because every string
in the result is a group key.** On DuckDB 1.5.5 + spatial, `ST_Extent_Agg`
corrupts the strings of a `histogram()` over VARCHAR computed in the same
aggregation, nondeterministically and with no error. An earlier design that
picked a majority name per code with `histogram` lost ~10,250 of 52,560
settlement names that way. Group keys are unaffected, and a 100k-row test
pins that. Adding any text-valued aggregate to this query means switching
the extent to `min`/`max` of `ST_X`/`ST_Y`, which is ~30% slower at gmina
level and ~10% at settlement level.

### `GET /stats`

```json
{
  "computed_at": "2026-09-27T11:40:02Z",
  "osm": {
    "replication_timestamp": "2026-09-27T11:38:00Z",
    "lag_seconds": 122,
    "addresses": { "node": 3272038, "way": 5416880, "relation": 3665 },
    "buildings": { "way": 17989407, "relation": 4896 },
    "former_buildings": 15407,
    "top_building_values": [ { "value": "yes", "count": 10780119 } ]
  },
  "datasets": [
    { "source": "prg", "comparable": 8615421, "unmatched": 518120,
      "imported_at": "…", "refreshed_at": null }
  ],
  "reports": [ { "source": "bdot10k", "status": "active", "count": 2 } ]
}
```

- `lag_seconds` is measured at `computed_at`, not at the time of the request.
- The response leaves every derived value (totals, percentages, which of the
  two load times is later) to the client, so each formula exists once.

### `GET /stats/areas`

| param | values | default |
|---|---|---|
| `level` | `gmina` \| `settlement` | `gmina` |
| `sort` | `ratio` \| `unmatched` | `ratio` |
| `order` | `desc` \| `asc` | `desc` |
| `in` | a TERC code of 2, 4 or 7 digits | none (the whole country) |
| `page` | 1–1,000,000 | `1` |
| `per_page` | 1–200 | `50` |

```json
{
  "level": "gmina",
  "sort": "ratio",
  "order": "desc",
  "in": null,
  "total_items": 2479,
  "page": 1,
  "per_page": 50,
  "items": [
    { "teryt": "1418043", "name": "Zwoleń", "teryt_gmina": "1418043",
      "gmina": "Zwoleń", "total": 9540, "unmatched": 3391,
      "bbox": [21.51, 51.29, 21.75, 51.43] }
  ]
}
```

At gmina level, `teryt_gmina`/`gmina` repeat `teryt`/`name`, so the client
needs no per-level branch. `bbox` is `[min lon, min lat, max lon, max lat]`
over the row's PRG address points, aggregated in the same query as the
counts. A malformed parameter is a 400 with a message. `page` is capped at
1,000,000. A page past the end is a 200 with an empty `items` and the real
`total_items`.

`in` keeps the rows whose `teryt_gmina` starts with the code, on both
`prg_addresses` and `prg_unmatched`, before grouping. A unit's code is a
prefix of everything inside it, so one clause serves all three levels with no
join. `total_items` counts the narrowed ranking. A well-formed code nothing
lies inside is a 200 with no items; anything else is a 400.

### `GET /stats/units`

```json
{ "units": [
    { "code": "02", "name": "dolnośląskie" },
    { "code": "0201", "name": "bolesławiecki" },
    { "code": "0201011", "name": "Bolesławiec" }
] }
```

All of `teryt_units`, by code, with no parameters: 109 kB, 24 kB gzipped. The
level is the code's length (2 województwo, 4 powiat, 7 gmina), so it is not
sent. The list is empty on a database whose PRG was loaded before the table
existed; the page then says so in place of suggestions, and a code in the
hash is shown bare.

## Page

- `web/stats.html` loads `style.css` for the tokens, plus its own `stats.css`
  and `stats.js`. The existing `ServeDir` fallback serves it, so there is no
  routing change. The page is `/stats.html` and the JSON is `/stats`.
- **Masthead link:** an `<a href="stats.html">` styled as a secondary masthead
  button, after `#display-options-btn`. It is a link because it navigates.
- **"← Mapa"** goes back in history when the referrer is same-origin, so the
  map's `map=` hash is restored. Otherwise it links to `./`.
- **Theme.** The page carries its own copy of the pre-paint theme script from
  `index.html`, as `unavailable.html` does, and the same `#theme-toggle`,
  driven by the map-free half of `app.js`'s theme code. The storage key is
  shared, so a choice holds on both pages. The theme gotcha in `CLAUDE.md`
  names the copies.
- **"Mapa" link per ranked area.** A relative `./#map=<zoom>/<lat>/<lon>`,
  the hash MapLibre already reads, so the map needs no change. The centre is
  the Mercator midpoint of `bbox`, and the zoom fits the extent into 85% of
  the current window, clamped to 5–17.
- **The ranking controls live in the hash**
  (`#level=settlement&sort=unmatched&order=asc&in=0201&page=3`), so a ranking
  can be shared. `in` is a code, never text; its name is looked up in the
  units list, which is fetched on the field's first focus, or at load when the
  hash carries `in`. This page is not the map, so the rule that view options stay out of
  the hash does not apply.
- Numbers use `Intl.NumberFormat("pl-PL")` and dates
  `toLocaleString("pl-PL")`.

## Configuration

```toml
[cache]
# max-age for GET /stats and /stats/areas. Both are queried per request
# (~0.1 s each on the Poland database), so this is their only caching.
stats_max_age_seconds = 300
```

## Tests

- The areas sum to the `prg` figures, at both levels.
- A settlement split across two gminy is a row per gmina, and the gmina level
  counts each half in its own gmina.
- A `w budowie` BDOT10k row is not counted as comparable.
- The order for each level, key and direction on a fixture with ties: only the
  chosen key flips, and ties keep the key columns ascending.
- Pagination is a partition: for each level, sort and order, concatenating the
  pages gives every row exactly once, and every page reports the same
  `total_items`.
- A page past the end is empty but still counts the rows.
- A row known only from a stale `prg_unmatched` copy is not listed.
- Long names survive the query at scale (the `ST_Extent_Agg` guard above).
- The load-time fields: an import stamp, a refresh and a never-loaded source.
- A 400 for each invalid parameter.
- Headers: a 200 carries the `stats` `Cache-Control`, and a 400 carries
  `no-store`.

## Later

These were considered and left out:

- **A minimum-size filter for the ranking.** The minimum size is what makes
  the settlement-level percentage ranking useful, per the consequence above.
- **Settlement search.** It needs a server-side suggestion source over
  `prg_addresses`' ~50,000 settlements, and a second narrowing parameter: a
  SIMC code is 7 digits like a gmina's TERC, but a different column.
- **A breakdown of the BDOT10k rows outside the comparison** by
  `KATEGORIAISTNIENIA`: `w budowie` 142,946, `nieczynny` 116,823, `zniszczony`
  75,926.
- **History.** A daily row per source would give a progress chart. It is the
  one thing here that cannot be recomputed later, and it would be the second
  table (after `object_reports`) that is not an `import full` away.
- **CSV export** and **a województwo level**. Województwo is the first two
  TERYT digits plus a static 16-entry name table.
- **Rankings for buildings.** Neither building table carries a settlement.
  See the appendix.

## Appendix: building attribution

Measured before settling on addresses only, and kept so the work is not
repeated.

- **EGIB attributes exactly, at no cost.**
  - `id_budynku` begins with the cadastral unit's TERYT code. All 17,589,332
    rows match `^[0-9]{6}_[0-9]\.`, giving 3,194 units.
  - Mapping to PRG's `teryt_gmina`:
    - types 1–3 map as they are;
    - types 4 and 5 (the urban and rural parts of a mixed gmina) map to `…3`;
    - types 8 and 9 (districts of Warsaw, Kraków and so on) map to the city's
      `…011`.
  - 99.97% of rows join. This attributes to a gmina only, never to a
    settlement.
- **BDOT10k has nothing to join on.** `PRZESTRZENNAZW` is a PZGiK namespace,
  not a unit.
  - The best proxy found: assign each building the majority PRG
    `teryt_gmina` of its ~140 m grid cell, falling back to a 5× coarser cell.
  - It agrees with EGIB's exact gmina for **99.16%** of buildings, and leaves
    0.46% unattributed.
  - It runs in ~2.5 s.
