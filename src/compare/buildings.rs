use anyhow::{Context, Result};
use duckdb::Connection;
use tracing::info;

use crate::compare::columns::classification_columns;
use crate::compare::in_transaction;
use crate::compare::rule::{
    BDOT10K_EKSPLOATOWANY_FILTER, reported_buildings_sql, suppressed_buildings_sql,
    unmatched_buildings_sql,
};
use crate::compare::totals;
use crate::dataset::{BDOT10K, DatasetSpec, EGIB};
use crate::tile_math::{cell_x_sql, cell_y_sql};
use crate::utils::format_duration;

/// Grid cell size in degrees for chunked spatial comparison (memory bound).
///
/// **Larger is faster, and that is the whole reason this is not smaller.**
/// Each cell's statement scans the *entire* source table sequentially and
/// tests every centroid against the cell -- the write-narrow guard below keeps
/// the centroid RTREE out of the plan, and the scan is what the full compare
/// wants anyway (see the per-cell-recompute gotcha in `CLAUDE.md`). So the
/// cost of finding a cell's rows is `cells x one table scan`, independent of
/// what the cell holds: ~0.6 s of wall time per cell on the national bdot10k
/// table, empty cells included. At 0.5° that was 252 scans and ~65% of all
/// CPU, with 52 of the cells writing no row at all. Measured with the release
/// binary's real `compare buildings` on the national tables, 8 threads -- the
/// grid loop alone, with the whole source's elapsed time in parentheses:
///
/// | step | cells | bdot10k @ 6.5 GB  | bdot10k @ 4 GB    | egib @ 6.5 GB |
/// |------|-------|-------------------|-------------------|---------------|
/// | 0.5° | 252   | 3m 59s (4m 20s)   | 4m 06s (4m 36s)   | (5m 03s)      |
/// | 1°   | 66    | 2m 02s (2m 18s)   | —                 | —             |
/// | 2°   | 24    | 1m 27s (1m 42s)   | 1m 42s (3m 10s)   | 1m 33s (2m 13s) |
///
/// **A smaller step does not buy the index back.** The RTREE only appears if
/// the source scan is wrapped in a `candidates` CTE, and then fetching a row
/// through it costs ~54 µs of CPU whatever the cell size -- the tables are not
/// spatially clustered on disk, so every fetch is effectively random. Over one
/// 2°x2° block that shape took 26.4 s at 0.5°, 29.1 s at 0.25° and 45.8 s at
/// 0.125°: constant CPU, falling parallelism.
///
/// What bounds it from above is memory: the anti-join's `DELIM_JOIN`
/// materializes every projected column of the cell's source rows, and the
/// `SPATIAL_JOIN` builds over the cell's `osm_buildings`. At 2° the densest
/// cell (18–20°E, 50–52°N, ~12 s) holds 2.7M bdot10k / 3.0M egib rows; peak
/// RSS stayed within ~0.4 GB of `memory_limit` at both 4 GB and 6.5 GB, for
/// both sources. Nothing larger was measured.
///
/// **Measure a change here with the release binary and the real statement, or
/// not at all.** Both shortcuts gave a confident wrong answer for this
/// constant: a stand-in `INSERT` projecting only the id columns lets DuckDB
/// prune the carried columns out of the `DELIM_JOIN` and understates memory;
/// and the *debug* binary pays ~2.7x per cell (1.66 s against 0.60 s for an
/// empty cell, with DuckDB itself compiled identically in both profiles),
/// which makes a small step look far worse and a large one far better than
/// they are.
///
/// The step is also, very slightly, a *result* input: `osm_buildings` is
/// filtered by the cell envelope, so a government building whose only covering
/// OSM building lies wholly in the neighbouring cell reads as unmatched.
/// Fewer cell edges mean fewer of those (485,314 unmatched bdot10k rows at
/// 0.5°, 485,311 at 1°, 485,310 at 2°).
const GRID_STEP: f64 = 2.0;

pub fn compare_bdot10k(conn: &Connection) -> Result<()> {
    compare_buildings(
        conn,
        &BDOT10K,
        "LOKALNYID",
        "bdot10k_unmatched",
        Some(BDOT10K_EKSPLOATOWANY_FILTER),
    )
}

pub fn compare_egib(conn: &Connection) -> Result<()> {
    compare_buildings(conn, &EGIB, "id_budynku", "egib_unmatched", None)
}

/// Takes a whole `spec` rather than a label plus a source-table name: they
/// cannot then be passed inconsistently (`spec.name` is the label, identical to
/// `match_dirty_cells.source`; `spec.table` is the live table), and the
/// user-report veto gets the key columns it correlates on from the same place.
fn compare_buildings(
    conn: &Connection,
    spec: &DatasetSpec,
    id_col: &str,
    dest: &str,
    extra_filter: Option<&str>,
) -> Result<()> {
    compare_buildings_with_cancel(
        conn,
        spec,
        id_col,
        dest,
        extra_filter,
        &crate::shutdown::is_requested,
    )
}

/// Does the actual work of `compare_buildings`, with the shutdown check
/// factored out behind `is_cancelled` so a test can inject a closure that
/// returns `true` on a chosen call rather than flipping the process-global
/// `shutdown::is_requested` flag (which would leak into every other test
/// running in the same process). The production entry point above always
/// passes `&crate::shutdown::is_requested`.
fn compare_buildings_with_cancel(
    conn: &Connection,
    spec: &DatasetSpec,
    id_col: &str,
    dest: &str,
    extra_filter: Option<&str>,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let label = spec.name;
    let source_table = spec.table;
    info!(source = label, "Comparing buildings against OSM");
    let t = std::time::Instant::now();

    // Clear and repopulate atomically: without the transaction the DELETE
    // commits on its own, so a failure anywhere in the grid loop below (or in
    // source_grid_extent) leaves `dest` empty rather than leaving the previous
    // comparison in place. See `compare::in_transaction` for the incident this
    // guards against.
    // Returned from the closure (`in_transaction` is generic in T) so the log
    // line below can reuse it for `suppressed_buildings_sql` rather than
    // recomputing an `ST_Extent_Agg` over the whole source table again.
    let extent = in_transaction(conn, label, || {
        conn.execute_batch(&format!("DELETE FROM {dest};"))
            .with_context(|| format!("Failed to clear {dest}"))?;

        let extent @ (min_x, min_y, max_x, max_y) =
            source_grid_extent(conn, source_table, GRID_STEP)
                .with_context(|| format!("Failed to compute source extent for {source_table}"))?;
        let cx = cell_x_sql("b.centroid");
        let cy = cell_y_sql("b.centroid");
        let cc = classification_columns(source_table);
        let select = format!("b.{id_col}, b.geom, {cx}, {cy}, now(), {}", cc.source_exprs);

        let mut y = min_y;
        while y < max_y {
            let mut x = min_x;
            while x < max_x {
                // The DELETE above already ran inside this same transaction
                // (see `in_transaction`'s doc comment: a full compare is a
                // clear-then-repopulate), so the only safe way to stop this
                // loop early is to fail the closure and let `in_transaction`
                // roll the DELETE back too -- leaving the previous `dest`
                // contents in place. `bail!` here, not `return Ok(())`.
                //
                // That is the opposite of how `update::osm::update` handles
                // the same flag, and deliberately so: that loop commits one
                // replication batch at a time and resumes from a metadata
                // stamp, so an early `Ok` there just means "less caught up,
                // pick up again next run." This grid loop has no such
                // checkpoint partway through -- an early `Ok` here would let
                // `in_transaction` COMMIT a `dest` missing every cell after
                // this one, which is exactly the silent-outage failure mode
                // (`/tiles`/`/package` answering with zero features) that
                // transaction exists to prevent. Don't "make this consistent"
                // with the OSM update path; the two loops resume differently,
                // so they must fail differently too.
                //
                // The message comes from `shutdown::SHUTDOWN_BAIL_MESSAGE`
                // rather than being retyped, so a cancellation noticed here
                // is indistinguishable from one noticed by
                // `shutdown::check_requested()` at any other seam. This
                // cannot call `check_requested()` itself: it must consult the
                // injected `is_cancelled`, not the process-global flag
                // directly, so that the test below can drive it.
                if is_cancelled() {
                    anyhow::bail!(crate::shutdown::SHUTDOWN_BAIL_MESSAGE);
                }
                let (x_hi, y_hi) = (x + GRID_STEP, y + GRID_STEP);
                let area = (x, y, x_hi, y_hi);
                let inner =
                    unmatched_buildings_sql(spec, source_table, &select, area, extra_filter);
                // Write-narrow: unmatched_buildings_sql's ST_Intersects test is
                // closed on all four cell edges, so a centroid exactly on a grid
                // line would satisfy two neighbouring cells' predicates. Restrict
                // the actual write to this cell's half-open interval so a
                // boundary row is written by exactly the cell that owns it (the
                // z14 analogue of this guard lives in
                // incremental::recompute_cell_in_txn).
                let cell_started = std::time::Instant::now();
                conn.execute_batch(&format!(
                    "INSERT INTO {dest} ({id_col}, geom, cell_x, cell_y, computed_at, {})
                     {inner}
                       AND ST_X(b.centroid) >= {x} AND ST_X(b.centroid) < {x_hi}
                       AND ST_Y(b.centroid) >= {y} AND ST_Y(b.centroid) < {y_hi};",
                    cc.dest_names
                ))
                .with_context(|| format!("Failed comparing {label} in cell ({x},{y})"))?;
                // Per-cell timing at debug level: the grid is nearly all of a
                // building compare's wall time, and without this the only
                // number an operator has is the total.
                tracing::debug!(
                    source = label,
                    x,
                    y,
                    elapsed_ms = cell_started.elapsed().as_millis() as u64,
                    "grid cell compared"
                );
                x += GRID_STEP;
            }
            y += GRID_STEP;
        }
        tracing::debug!(source = label, elapsed = %format_duration(t.elapsed()), "grid loop done");
        // Inside the same transaction as the rows it counts, so a cell's
        // numerator and denominator always come from one comparison.
        totals::rebuild_all_in_txn(conn, label)
            .with_context(|| format!("Failed to rebuild cell totals for {label}"))?;
        tracing::debug!(source = label, elapsed = %format_duration(t.elapsed()), "cell totals rebuilt");
        Ok(extent)
    })?;
    tracing::debug!(source = label, elapsed = %format_duration(t.elapsed()), "committed");

    // total is now accurate: the grid above covers the source table's full
    // extent (source_grid_extent), and the write-narrow guard means each row
    // is written by exactly one cell, so matched = total - unmatched - suppressed
    // holds. `total` applies the same extra_filter as the comparison itself
    // (e.g. BDOT10K_EKSPLOATOWANY_FILTER), so a filtered-out row is neither
    // matched nor unmatched nor suppressed -- it's simply excluded from every
    // count. `total` itself is unchanged by suppression: a suppressed building
    // is still a government building, just one OSM has recorded as gone.
    let total_where = extra_filter
        .map(|f| format!("WHERE {f}"))
        .unwrap_or_default();
    let (total, unmatched): (i64, i64) = conn.query_row(
        &format!(
            "SELECT (SELECT COUNT(*) FROM {source_table} b {total_where}), \
                    (SELECT COUNT(*) FROM {dest})"
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // Gated on an instant read of osm_former_buildings so a database that has
    // not been re-imported -- and every test fixture -- pays nothing extra.
    //
    // Unlike the grid loop above, this is ONE query over the whole extent
    // rather than one per grid cell. That only works because
    // suppressed_buildings_sql filters by the former-building veto first and
    // runs the expensive osm_buildings anti-join over just those candidates
    // -- see its doc comment. Spelled flat (the shape unmatched_buildings_sql
    // uses, which is correct *per cell*), the identical query OOMs at
    // national scale against a 4 GB memory_limit. Measured whole-extent on
    // the Poland database: bdot10k 4.7 s / 3.8 GB, egib 4.9 s / 3.9 GB.
    //
    // So if this ever needs changing, the fix is not to widen the memory
    // limit -- it is to chunk this the way the grid loop chunks its own copy
    // of the anti-join.
    let former_building_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM osm_former_buildings", [], |r| {
            r.get(0)
        })
        .context("Failed to count osm_former_buildings")?;
    let suppressed: i64 = if former_building_rows > 0 {
        conn.query_row(
            &suppressed_buildings_sql(source_table, "COUNT(*)", extent, extra_filter),
            [],
            |r| r.get(0),
        )
        .with_context(|| format!("Failed to count suppressed rows for {label}"))?
    } else {
        0
    };
    // The user-report veto's own count, on exactly the same terms as
    // `suppressed` above: gated on an instant read so a database with no
    // reports (and every test fixture) pays nothing, and phrased CTE-first by
    // `reported_buildings_sql` so the whole-extent run cannot reproduce the
    // OOM described there. Without this number, a reported building would be
    // indistinguishable from a matched one in this log line -- and this is
    // precisely the number an operator checks to see whether reporting works.
    let active_reports: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM object_reports WHERE source = ? AND status = ?",
            duckdb::params![spec.name, crate::compare::rule::REPORT_ACTIVE],
            |r| r.get(0),
        )
        .context("Failed to count active object_reports")?;
    let reported: i64 = if active_reports > 0 {
        conn.query_row(
            &reported_buildings_sql(spec, source_table, "COUNT(*)", extent, extra_filter),
            [],
            |r| r.get(0),
        )
        .with_context(|| format!("Failed to count reported rows for {label}"))?
    } else {
        0
    };
    info!(
        source = label, total, unmatched, suppressed, reported,
        matched = total - unmatched - suppressed - reported,
        elapsed = %format_duration(t.elapsed()),
        "buildings comparison complete"
    );
    Ok(())
}

/// Grid-aligned bounding box covering every row in `source_table`, snapped
/// outward to `grid_step` multiples so the chunked scan above never misses a
/// row whose centroid falls outside the historical Poland bbox (a coordinate
/// error, a stray far-away row). The upper bound is snapped via
/// `floor(x/step)*step + step` rather than `ceil`, so a value that already
/// sits exactly on a grid line still gets a full extra cell of headroom
/// (`ceil` would leave it exactly on the loop's exclusive upper bound,
/// covered by nothing). Falls back to the historical (14.0, 49.0, 25.0, 55.0)
/// box when the table has no rows — the grid still iterates, every cell just
/// matches nothing.
///
/// Because the extent comes from the data rather than a fixed box, a single
/// malformed coordinate inflates the cell count that the caller then iterates
/// one query at a time. Even a *valid* WGS84 outlier is enough: one row at
/// (-180, -90) turns Poland's 24 cells into 7 519. So the cell count is
/// capped — exceeding the cap means the source table holds a coordinate no
/// grid should be built around, which is a data fault worth failing loudly on
/// rather than silently skipping (the old hardcoded box) or grinding for hours
/// (an uncapped derived box).
fn source_grid_extent(
    conn: &Connection,
    source_table: &str,
    grid_step: f64,
) -> Result<(f64, f64, f64, f64)> {
    const FALLBACK: (f64, f64, f64, f64) = (14.0, 49.0, 25.0, 55.0);
    /// How many times the historical Poland grid the cap allows — generous
    /// enough for a stray row a few degrees outside the country, far short of
    /// the blow-up a wild coordinate causes.
    const MAX_GRID_MULTIPLE: f64 = 8.0;
    // Derived from `grid_step` rather than written as a cell count: every cell
    // costs a full scan of the source table whether or not it holds anything,
    // so the cap is a bound on wasted scans and has to stay a small multiple
    // of the real grid at whatever step is in use. A literal would have to be
    // rescaled by hand on every step change, and forgetting only ever loosens
    // it.
    let (fx1, fy1, fx2, fy2) = FALLBACK;
    let max_grid_cells =
        MAX_GRID_MULTIPLE * ((fx2 - fx1) / grid_step).ceil() * ((fy2 - fy1) / grid_step).ceil();

    let row: (Option<f64>, Option<f64>, Option<f64>, Option<f64>) = conn.query_row(
        &format!(
            "SELECT ST_XMin(ST_Extent_Agg(geom)), ST_YMin(ST_Extent_Agg(geom)),
                    ST_XMax(ST_Extent_Agg(geom)), ST_YMax(ST_Extent_Agg(geom))
             FROM {source_table}"
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let (x1, y1, x2, y2) = match row {
        (Some(x1), Some(y1), Some(x2), Some(y2)) => (
            (x1 / grid_step).floor() * grid_step,
            (y1 / grid_step).floor() * grid_step,
            (x2 / grid_step).floor() * grid_step + grid_step,
            (y2 / grid_step).floor() * grid_step + grid_step,
        ),
        // Any NULL means no rows with geometry; NaN would poison the loop
        // bounds, so treat it the same way.
        _ => return Ok(FALLBACK),
    };
    if !(x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite()) {
        anyhow::bail!(
            "{source_table} extent is not finite ({x1},{y1},{x2},{y2}) — \
             the table holds a malformed geometry"
        );
    }
    let cells = ((x2 - x1) / grid_step) * ((y2 - y1) / grid_step);
    if cells > max_grid_cells {
        anyhow::bail!(
            "{source_table} spans ({x1},{y1},{x2},{y2}) — {cells:.0} grid cells at \
             {grid_step}°, over the {max_grid_cells:.0}-cell cap. A coordinate far \
             outside Poland is almost certainly bad source data; find and fix that row \
             (its centroid is at one of the extent corners) before comparing."
        );
    }
    Ok((x1, y1, x2, y2))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::db::init_db;

    /// Spin up an in-memory DuckDB with spatial loaded and the serving
    /// tables created by `init_db`. `osm_buildings` is seeded with a single
    /// polygon; tests create their own government-source table.
    fn setup() -> Connection {
        let init = vec!["INSTALL spatial".to_string(), "LOAD spatial".to_string()];
        let conn = init_db(Path::new(":memory:"), &init, None).unwrap();
        conn.execute_batch(
            "INSERT INTO osm_buildings VALUES
                 (1, 'way', NULL, ST_MakeEnvelope(20.0, 52.0, 20.001, 52.001));",
        )
        .unwrap();
        conn
    }

    #[test]
    fn writes_only_unmatched_rows_with_cell_tags() {
        let conn = setup();
        conn.execute_batch(
            "CREATE TABLE bdot10k_buildings (PRZESTRZENNAZW VARCHAR, LOKALNYID VARCHAR, geom GEOMETRY, centroid GEOMETRY,
                 PRZEWAZAJACAFUNKCJABUDYNKU VARCHAR, FUNKCJAOGOLNABUDYNKU VARCHAR, LICZBAKONDYGNACJI SMALLINT,
                 KATEGORIAISTNIENIA VARCHAR DEFAULT 'eksploatowany',
                 NAZWA VARCHAR, FSBUD VARCHAR, INFORMACJADODATKOWA VARCHAR, KODKST TINYINT,
                 ZRODLODANYCHGEOMETRYCZNYCH VARCHAR);
             INSERT INTO bdot10k_buildings (LOKALNYID, geom) VALUES
                 ('inside', ST_MakeEnvelope(20.0002,52.0002,20.0008,52.0008)),
                 ('lonely', ST_MakeEnvelope(21.0,52.2,21.001,52.201));
             UPDATE bdot10k_buildings SET centroid = ST_Centroid(geom);",
        )
        .unwrap();
        compare_bdot10k(&conn).unwrap();
        let ids: Vec<String> = {
            let mut s = conn
                .prepare("SELECT LOKALNYID FROM bdot10k_unmatched ORDER BY LOKALNYID")
                .unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            ids,
            vec!["lonely".to_string()],
            "only the uncontained building is stored"
        );
        let (cx, cy): (i32, i32) = conn
            .query_row("SELECT cell_x, cell_y FROM bdot10k_unmatched", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let (ex, ey) =
            crate::tile_math::lonlat_to_tile(21.0005, 52.2005, crate::tile_math::CHANGE_CELL_ZOOM);
        assert_eq!((cx as u32, cy as u32), (ex, ey));
    }

    #[test]
    fn compare_is_idempotent() {
        let conn = setup();
        conn.execute_batch(
            "CREATE TABLE bdot10k_buildings (PRZESTRZENNAZW VARCHAR, LOKALNYID VARCHAR, geom GEOMETRY, centroid GEOMETRY,
                 PRZEWAZAJACAFUNKCJABUDYNKU VARCHAR, FUNKCJAOGOLNABUDYNKU VARCHAR, LICZBAKONDYGNACJI SMALLINT,
                 KATEGORIAISTNIENIA VARCHAR DEFAULT 'eksploatowany',
                 NAZWA VARCHAR, FSBUD VARCHAR, INFORMACJADODATKOWA VARCHAR, KODKST TINYINT,
                 ZRODLODANYCHGEOMETRYCZNYCH VARCHAR);
             INSERT INTO bdot10k_buildings (LOKALNYID, geom) VALUES ('lonely', ST_MakeEnvelope(21.0,52.2,21.001,52.201));
             UPDATE bdot10k_buildings SET centroid = ST_Centroid(geom);",
        )
        .unwrap();
        compare_bdot10k(&conn).unwrap();
        compare_bdot10k(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM bdot10k_unmatched", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "re-running compare must not duplicate rows");
    }

    /// Deriving the grid from the data means one wild coordinate would
    /// otherwise inflate a 24-cell grid into thousands of
    /// one-full-table-scan-each iterations. A row at the WGS84 extreme is a valid
    /// geometry, so nothing upstream rejects it -- the cap must, and the error
    /// must point at the extent so the operator can find the bad row.
    #[test]
    fn refuses_to_build_a_grid_around_a_wild_coordinate() {
        let conn = setup();
        conn.execute_batch(
            "CREATE TABLE bdot10k_buildings (PRZESTRZENNAZW VARCHAR, LOKALNYID VARCHAR, geom GEOMETRY, centroid GEOMETRY,
                 PRZEWAZAJACAFUNKCJABUDYNKU VARCHAR, FUNKCJAOGOLNABUDYNKU VARCHAR, LICZBAKONDYGNACJI SMALLINT,
                 KATEGORIAISTNIENIA VARCHAR DEFAULT 'eksploatowany',
                 NAZWA VARCHAR, FSBUD VARCHAR, INFORMACJADODATKOWA VARCHAR, KODKST TINYINT,
                 ZRODLODANYCHGEOMETRYCZNYCH VARCHAR);
             INSERT INTO bdot10k_buildings (LOKALNYID, geom) VALUES
                 ('sane', ST_MakeEnvelope(20.0,52.0,20.001,52.001)),
                 ('wild', ST_MakeEnvelope(-180.0,-90.0,-179.999,-89.999));
             UPDATE bdot10k_buildings SET centroid = ST_Centroid(geom);",
        )
        .unwrap();
        // `{:#}` walks the whole anyhow chain -- the caller wraps this in
        // with_context, so plain Display would only show the outer message.
        let err = format!("{:#}", compare_bdot10k(&conn).unwrap_err());
        assert!(
            err.contains("grid cells") && err.contains("cap"),
            "the error must name the cell count and the cap, got: {err}"
        );
    }

    /// A row whose centroid falls outside the old hardcoded (14,49,25,55)
    /// Poland bbox (a coordinate error, a stray 0/0-adjacent row) must still
    /// be compared -- the grid now derives its extent from the source table
    /// instead of a fixed box, so `compare full` and the incremental path
    /// (which has no extent restriction at all) agree on every row. A stray row
    /// a few degrees out stays well under the cell cap that
    /// `refuses_to_build_a_grid_around_a_wild_coordinate` pins.
    #[test]
    fn covers_a_row_outside_the_historical_poland_bbox() {
        let conn = setup();
        conn.execute_batch(
            "CREATE TABLE bdot10k_buildings (PRZESTRZENNAZW VARCHAR, LOKALNYID VARCHAR, geom GEOMETRY, centroid GEOMETRY,
                 PRZEWAZAJACAFUNKCJABUDYNKU VARCHAR, FUNKCJAOGOLNABUDYNKU VARCHAR, LICZBAKONDYGNACJI SMALLINT,
                 KATEGORIAISTNIENIA VARCHAR DEFAULT 'eksploatowany',
                 NAZWA VARCHAR, FSBUD VARCHAR, INFORMACJADODATKOWA VARCHAR, KODKST TINYINT,
                 ZRODLODANYCHGEOMETRYCZNYCH VARCHAR);
             INSERT INTO bdot10k_buildings (LOKALNYID, geom) VALUES
                 ('stray', ST_MakeEnvelope(30.0,60.0,30.001,60.001));
             UPDATE bdot10k_buildings SET centroid = ST_Centroid(geom);",
        )
        .unwrap();
        compare_bdot10k(&conn).unwrap();
        let ids: Vec<String> = {
            let mut s = conn
                .prepare("SELECT LOKALNYID FROM bdot10k_unmatched")
                .unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            ids,
            vec!["stray".to_string()],
            "a row outside the hardcoded bbox must still be compared"
        );
    }

    /// Regression test for the write-narrow guard (re-added equivalent of the
    /// test deleted in the Task 4 refactor -- see
    /// `git show 0331d15^:src/compare/buildings.rs`, which asserted 2 rows
    /// before the chunked scan's write predicate was narrowed). A centroid
    /// exactly on a grid line satisfies both neighbouring cells'
    /// ST_Intersects envelope test (closed on all four edges), so without a
    /// narrow write it would be inserted twice.
    ///
    /// The fixture is tied to `GRID_STEP` and says so: at a step where 16.0 is
    /// not a grid line the row is interior to one cell, and this would pass
    /// without exercising the guard at all.
    #[test]
    fn compare_chunked_duplicates_source_on_cell_boundary() {
        assert_eq!(
            (16.0 / GRID_STEP).fract(),
            0.0,
            "this fixture puts a centroid on x = 16.0, which must be a grid line; \
             move it when GRID_STEP changes"
        );
        let conn = setup();
        conn.execute_batch(
            "CREATE TABLE bdot10k_buildings (PRZESTRZENNAZW VARCHAR, LOKALNYID VARCHAR, geom GEOMETRY, centroid GEOMETRY,
                 PRZEWAZAJACAFUNKCJABUDYNKU VARCHAR, FUNKCJAOGOLNABUDYNKU VARCHAR, LICZBAKONDYGNACJI SMALLINT,
                 KATEGORIAISTNIENIA VARCHAR DEFAULT 'eksploatowany',
                 NAZWA VARCHAR, FSBUD VARCHAR, INFORMACJADODATKOWA VARCHAR, KODKST TINYINT,
                 ZRODLODANYCHGEOMETRYCZNYCH VARCHAR);
             INSERT INTO bdot10k_buildings (LOKALNYID, geom) VALUES
                 -- centroid (16.0, 52.25): x = 16.0 is the boundary between
                 -- the [14.0,16.0) and [16.0,18.0) grid columns; y = 52.25 is
                 -- safely mid-row.
                 ('boundary', ST_MakeEnvelope(15.9998,52.2498,16.0002,52.2502)),
                 -- Widens the source extent so both neighbouring columns are
                 -- actually scanned by the chunked loop -- a lone boundary
                 -- point would otherwise sit at a tightly-snapped extent edge
                 -- and only ever be scanned by one cell, which would not
                 -- exercise the guard at all.
                 ('anchor', ST_MakeEnvelope(14.05,52.25,14.06,52.26));
             UPDATE bdot10k_buildings SET centroid = ST_Centroid(geom);
             -- Set outright rather than derived: ST_Centroid of those decimal
             -- corners is not guaranteed to land on 16.0 to the last bit, and
             -- a centroid a hair off the line is interior to one cell.
             UPDATE bdot10k_buildings SET centroid = ST_Point(16.0, 52.25)
              WHERE LOKALNYID = 'boundary';",
        )
        .unwrap();
        let on_both: bool = conn
            .query_row(
                "SELECT ST_Intersects(centroid, ST_MakeEnvelope(14, 52, 16, 54))
                    AND ST_Intersects(centroid, ST_MakeEnvelope(16, 52, 18, 54))
                 FROM bdot10k_buildings WHERE LOKALNYID = 'boundary'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            on_both,
            "the fixture must satisfy both neighbouring cells' envelope tests, \
             or the write-narrow guard is not what makes the count 1"
        );
        compare_bdot10k(&conn).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bdot10k_unmatched WHERE LOKALNYID = 'boundary'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 1,
            "a centroid exactly on a grid boundary must be written by exactly one cell"
        );
    }

    /// `compare bdot10k` only ever considers `KATEGORIAISTNIENIA =
    /// 'eksploatowany'` rows a government building to compare at all -- an
    /// unmatched "w budowie" (under construction) building must never be
    /// served, and must not count towards `total`/`matched` either (see the
    /// `total_where` comment above).
    #[test]
    fn excludes_non_eksploatowany_buildings_from_unmatched_and_totals() {
        let conn = setup();
        conn.execute_batch(
            "CREATE TABLE bdot10k_buildings (PRZESTRZENNAZW VARCHAR, LOKALNYID VARCHAR, geom GEOMETRY, centroid GEOMETRY,
                 PRZEWAZAJACAFUNKCJABUDYNKU VARCHAR, FUNKCJAOGOLNABUDYNKU VARCHAR, LICZBAKONDYGNACJI SMALLINT,
                 KATEGORIAISTNIENIA VARCHAR,
                 NAZWA VARCHAR, FSBUD VARCHAR, INFORMACJADODATKOWA VARCHAR, KODKST TINYINT,
                 ZRODLODANYCHGEOMETRYCZNYCH VARCHAR);
             INSERT INTO bdot10k_buildings (LOKALNYID, geom, KATEGORIAISTNIENIA) VALUES
                 ('lonely', ST_MakeEnvelope(21.0,52.2,21.001,52.201), 'eksploatowany'),
                 ('under_construction', ST_MakeEnvelope(22.0,53.2,22.001,53.201), 'w budowie');
             UPDATE bdot10k_buildings SET centroid = ST_Centroid(geom);",
        )
        .unwrap();
        compare_bdot10k(&conn).unwrap();
        let ids: Vec<String> = {
            let mut s = conn
                .prepare("SELECT LOKALNYID FROM bdot10k_unmatched ORDER BY LOKALNYID")
                .unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            ids,
            vec!["lonely".to_string()],
            "the under-construction building must never be served as unmatched"
        );
    }

    /// A government building fully covered by an `osm_former_buildings`
    /// polygon must be excluded from `bdot10k_unmatched` (the veto), but --
    /// unlike a `KATEGORIAISTNIENIA`-filtered row -- it must still count
    /// towards `cell_totals`: it is comparable, and OSM has effectively
    /// handled it, so the denominator stays whole (see `compare::totals`'s
    /// module doc for why `cell_totals` deliberately does not mirror the veto).
    /// The second assertion below is what pins that denominator decision.
    #[test]
    fn former_building_excludes_from_unmatched_but_cell_totals_still_counts_it() {
        let conn = setup();
        conn.execute_batch(
            "CREATE TABLE bdot10k_buildings (PRZESTRZENNAZW VARCHAR, LOKALNYID VARCHAR, geom GEOMETRY, centroid GEOMETRY,
                 PRZEWAZAJACAFUNKCJABUDYNKU VARCHAR, FUNKCJAOGOLNABUDYNKU VARCHAR, LICZBAKONDYGNACJI SMALLINT,
                 KATEGORIAISTNIENIA VARCHAR DEFAULT 'eksploatowany',
                 NAZWA VARCHAR, FSBUD VARCHAR, INFORMACJADODATKOWA VARCHAR, KODKST TINYINT,
                 ZRODLODANYCHGEOMETRYCZNYCH VARCHAR);
             INSERT INTO bdot10k_buildings (LOKALNYID, geom) VALUES
                 ('suppressed', ST_MakeEnvelope(22.0,53.0,22.001,53.001));
             UPDATE bdot10k_buildings SET centroid = ST_Centroid(geom);
             INSERT INTO osm_former_buildings VALUES
                 (1, 'way', 'demolished:building', 'yes',
                  ST_MakeEnvelope(21.9999,52.9999,22.0011,53.0011));",
        )
        .unwrap();
        compare_bdot10k(&conn).unwrap();

        let ids: Vec<String> = {
            let mut s = conn
                .prepare("SELECT LOKALNYID FROM bdot10k_unmatched")
                .unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            ids,
            Vec::<String>::new(),
            "a former-building-suppressed building must never be served as unmatched"
        );

        let (cx, cy) =
            crate::tile_math::lonlat_to_tile(22.0005, 53.0005, crate::tile_math::CHANGE_CELL_ZOOM);
        let total: i64 = conn
            .query_row(
                "SELECT total FROM cell_totals WHERE source = 'bdot10k' AND cell_x = ? AND cell_y = ?",
                duckdb::params![cx as i32, cy as i32],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            total, 1,
            "a suppressed building must still count towards its cell's denominator"
        );
    }

    /// A compare cancelled partway through the grid must roll back the
    /// DELETE too, leaving `dest` exactly as a previous run left it -- not
    /// emptied, and not partially repopulated. This is the failure mode
    /// `compare::in_transaction`'s doc comment calls out by name (an
    /// in-progress compare silently serving zero features), just triggered
    /// by a shutdown request instead of a query error.
    ///
    /// `crate::shutdown::is_requested()` reads a process-global static, which
    /// a test cannot flip without leaking `true` into every other test in
    /// this binary (they all share one process). So this drives
    /// `compare_buildings_with_cancel` directly with an injected closure
    /// that goes true partway through the grid scan, the same shape
    /// `compare::drain::drain_batch` already uses for its own
    /// `is_cancelled: &dyn Fn() -> bool` parameter.
    #[test]
    fn cancelled_compare_leaves_the_previous_contents_intact() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let conn = setup();
        conn.execute_batch(
            "CREATE TABLE bdot10k_buildings (PRZESTRZENNAZW VARCHAR, LOKALNYID VARCHAR, geom GEOMETRY, centroid GEOMETRY,
                 PRZEWAZAJACAFUNKCJABUDYNKU VARCHAR, FUNKCJAOGOLNABUDYNKU VARCHAR, LICZBAKONDYGNACJI SMALLINT,
                 KATEGORIAISTNIENIA VARCHAR DEFAULT 'eksploatowany',
                 NAZWA VARCHAR, FSBUD VARCHAR, INFORMACJADODATKOWA VARCHAR, KODKST TINYINT,
                 ZRODLODANYCHGEOMETRYCZNYCH VARCHAR);
             -- Two buildings several degrees apart so source_grid_extent
             -- spans many grid cells (12 at 2 degrees) -- the cancellation below fires
             -- partway through that scan, not on the only cell there is.
             INSERT INTO bdot10k_buildings (LOKALNYID, geom) VALUES
                 ('lonely_a', ST_MakeEnvelope(14.05,49.05,14.06,49.06)),
                 ('lonely_b', ST_MakeEnvelope(20.0,52.0,20.001,52.001));
             UPDATE bdot10k_buildings SET centroid = ST_Centroid(geom);
             -- Simulates a previous, already-committed comparison run --
             -- the row a cancelled compare must NOT discard.
             INSERT INTO bdot10k_unmatched (LOKALNYID, geom, cell_x, cell_y, computed_at)
             VALUES ('previous_run', ST_Point(21.0, 52.0), 9147, 5411, now());",
        )
        .unwrap();

        // False for the first three grid cells scanned, true from then on --
        // exercises a mid-grid cancellation rather than an immediate one.
        let calls = AtomicUsize::new(0);
        let is_cancelled = || calls.fetch_add(1, Ordering::SeqCst) >= 3;

        let err = compare_buildings_with_cancel(
            &conn,
            &BDOT10K,
            "LOKALNYID",
            "bdot10k_unmatched",
            Some(BDOT10K_EKSPLOATOWANY_FILTER),
            &is_cancelled,
        )
        .expect_err("a cancelled compare must return Err, not Ok(())");
        assert!(
            format!("{err:#}").contains(crate::shutdown::SHUTDOWN_BAIL_MESSAGE),
            "got: {err:#}"
        );

        let rows: Vec<String> = {
            let mut s = conn
                .prepare("SELECT LOKALNYID FROM bdot10k_unmatched")
                .unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            rows,
            vec!["previous_run".to_string()],
            "a cancelled compare must roll back its DELETE, not leave the \
             serving table empty or partially repopulated"
        );
    }
}
