//! Checks that a PRG load was read with the right axis order, by comparing
//! its extent with Poland's.
//!
//! PRG publishes points in EPSG:2180 and has used both orders of the two
//! numbers: easting first until August 2026, northing first since 2026-10-01.
//! `prg_convert` tells them apart by each point's `srsName`. This module is
//! the backstop for a file whose declaration is wrong, a parser that guesses
//! wrong, or reversed arguments in [`POINT_SQL`]. Measurements and queries:
//! `docs/prg_axis_order.md`.

use anyhow::{Context, Result, bail};
use duckdb::Connection;

/// The point `materialize_into` stores, as an expression over the raw table.
/// One constant shared by the load select and [`check_axis_order`], so the
/// check judges the geometry that is about to be stored -- which is what makes
/// it catch reversed arguments here as well as a mirrored parse.
pub const POINT_SQL: &str = "ST_Point(dlugosc_geograficzna, szerokosc_geograficzna)";

/// A lon/lat box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Extent {
    pub lon_min: f64,
    pub lon_max: f64,
    pub lat_min: f64,
    pub lat_max: f64,
}

/// Poland, with a little room: its real extremes are 14.12°E, 24.15°E,
/// 49.00°N and 54.84°N.
///
/// Measured on the full national snapshots of 2026-08-14 (8,608,828 rows) and
/// 2026-10-01 (8,624,770 rows): **0 rows outside**, observed extent
/// 14.129-24.131°E, 49.063-54.835°N. The October snapshot read with its two
/// EPSG:2180 numbers exchanged spans 13.405-22.996°E, 49.413-55.586°N and puts
/// 316,280 rows (3.67%) outside -- north into the Baltic and west into
/// Germany.
///
/// A box for the whole country is deliberately all there is. Mirroring is a
/// reflection across the line easting = northing, which runs from Wrocław to
/// Mazury, so most mirrored points stay inside Poland. Only four voivodeships
/// send any row out of the box (lubelskie 34.1%, podlaskie 24.4%,
/// podkarpackie 3.7%, małopolskie 2.0%); **the other twelve mirror entirely
/// onto Polish territory, and a mirrored load of one of them alone passes
/// this check.**
pub const POLAND_EXTENT: Extent = Extent {
    lon_min: 14.0,
    lon_max: 24.2,
    lat_min: 49.0,
    lat_max: 54.9,
};

/// Largest share of rows that may fall outside [`POLAND_EXTENT`]. Real data
/// measures 0 and a mirrored national snapshot 3.67%, 37 times over. It is not
/// zero so that one stray record cannot fail a national load.
pub const MAX_OUTSIDE_SHARE: f64 = 0.001;

/// How the error from [`check_axis_order`] begins. Tests match on this and on
/// one of the two hints below, never on the counts that follow.
pub const AXIS_ORDER_BAIL_MESSAGE: &str = "PRG coordinates do not fit Poland's extent";

/// Ends the error when the load fits once the two EPSG:2180 numbers are
/// exchanged: the file was read in the wrong axis order.
pub const EXCHANGED_FITS_HINT: &str = "The same rows fit with their two EPSG:2180 coordinates exchanged, so the file was read in \
     the wrong axis order. Set `coordinate_order` under `[prg]` in the config, or pass \
     `--coordinate-order`, to `xy` (easting first) or `yx` (northing first) to force the other one.";

/// Ends the error when exchanging the two numbers does not help either.
pub const NEITHER_FITS_HINT: &str = "Exchanging their two EPSG:2180 coordinates does not make them fit either, so this is not \
     a plain axis swap.";

fn inside_sql(point: &str, extent: &Extent) -> String {
    format!(
        "(ST_X({point}) BETWEEN {} AND {} AND ST_Y({point}) BETWEEN {} AND {})",
        extent.lon_min, extent.lon_max, extent.lat_min, extent.lat_max
    )
}

fn over_tolerance(outside: i64, rows: i64) -> bool {
    rows > 0 && outside as f64 > rows as f64 * MAX_OUTSIDE_SHARE
}

/// Fail unless the rows of `raw_table` selected by `row_filter` (the load
/// select's own `WHERE`) lie inside [`POLAND_EXTENT`], allowing
/// [`MAX_OUTSIDE_SHARE`] of them to stray.
///
/// The common case is one streaming aggregate over three columns with no
/// per-row state: measured 0.14 s on 8.2M rows, and it completes under
/// `memory_limit = '150MB'`. Everything else runs only on the way to an
/// error:
///
/// - The example ids come from their own `LIMIT` query. Collecting them with
///   `list(...) FILTER (...)` inside the aggregate would hold every outside id
///   -- 316,280 strings for a mirrored snapshot, every row of the table for
///   reversed [`POINT_SQL`] arguments -- and is the one way this check could
///   cost real memory.
/// - The exchanged reading is reprojected by DuckDB only to choose between
///   the two hints. It is never stored: DuckDB's projection differs from
///   `prg_convert`'s in the last bits for a third of all rows.
///
/// Nothing is repaired here. A load that does not fit is an operator's call,
/// and the fix (`prg.coordinate_order`) goes back through the parser.
pub fn check_axis_order(conn: &Connection, raw_table: &str, row_filter: &str) -> Result<()> {
    let inside = inside_sql("pt", &POLAND_EXTENT);
    let (rows, outside, lon_min, lon_max, lat_min, lat_max): (
        i64,
        i64,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
    ) = conn
        .query_row(
            &format!(
                "SELECT count(*), count(*) FILTER (WHERE NOT {inside}),
                        min(ST_X(pt)), max(ST_X(pt)), min(ST_Y(pt)), max(ST_Y(pt))
                 FROM (SELECT {POINT_SQL} AS pt FROM {raw_table} WHERE {row_filter})"
            ),
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .with_context(|| format!("Failed to measure the extent of {raw_table}"))?;

    if !over_tolerance(outside, rows) {
        if outside > 0 {
            tracing::warn!(
                outside,
                rows,
                "PRG rows outside Poland's extent, within tolerance; loading them as they are"
            );
        }
        return Ok(());
    }

    let mut example_ids = Vec::new();
    let mut stmt = conn
        .prepare(&format!(
            "SELECT lokalny_id
             FROM (SELECT lokalny_id, {POINT_SQL} AS pt FROM {raw_table} WHERE {row_filter})
             WHERE NOT {inside}
             LIMIT {}",
            crate::dataset::MAX_EXAMPLE_IDS
        ))
        .with_context(|| format!("Failed to prepare the outside-extent id scan on {raw_table}"))?;
    for id in stmt
        .query_map([], |r| r.get::<_, String>(0))
        .with_context(|| format!("Failed to scan outside-extent ids in {raw_table}"))?
    {
        example_ids.push(id.context("Failed to read an outside-extent id")?);
    }

    // `ex IS NULL` counts as outside: a row with no EPSG:2180 pair cannot be
    // shown to fit. A failure of this query must not replace the error it is
    // only decorating, so it degrades to the neutral hint.
    let exchanged_inside = inside_sql("ex", &POLAND_EXTENT);
    let outside_exchanged: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT count(*) FILTER (WHERE ex IS NULL OR NOT {exchanged_inside})
                 FROM (SELECT ST_Transform(ST_Point(y_epsg_2180, x_epsg_2180),
                                           'EPSG:2180', 'EPSG:4326', always_xy := true) AS ex
                       FROM {raw_table} WHERE {row_filter})"
            ),
            [],
            |r| r.get(0),
        )
        .map_err(|e| tracing::warn!(error = %e, "could not evaluate the exchanged axis order"))
        .ok();

    let hint = match outside_exchanged {
        Some(n) if !over_tolerance(n, rows) => EXCHANGED_FITS_HINT.to_string(),
        Some(n) => format!("{NEITHER_FITS_HINT} ({n} rows outside that way.)"),
        None => NEITHER_FITS_HINT.to_string(),
    };

    let fmt = |v: Option<f64>| v.map_or("?".to_string(), |v| format!("{v:.4}"));
    bail!(
        "{AXIS_ORDER_BAIL_MESSAGE}: {outside} of {rows} rows fall outside lon {}..{}, lat {}..{} \
         (observed lon {}..{}, lat {}..{}; e.g. {}). {hint}",
        POLAND_EXTENT.lon_min,
        POLAND_EXTENT.lon_max,
        POLAND_EXTENT.lat_min,
        POLAND_EXTENT.lat_max,
        fmt(lon_min),
        fmt(lon_max),
        fmt(lat_min),
        fmt(lat_max),
        example_ids.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::db::init_db;

    const ALL_ROWS: &str = "TRUE";

    fn conn() -> Connection {
        let init = vec!["INSTALL spatial".to_string(), "LOAD spatial".to_string()];
        init_db(Path::new(":memory:"), &init, None).unwrap()
    }

    /// A raw-shaped table of `n` points on a diagonal starting at the
    /// EPSG:2180 pair (`easting`, `northing`), 10 m apart. lon/lat come from
    /// the pair `as_read`: `(easting, northing)` for a correct parse,
    /// `(northing, easting)` for a mirrored one -- so the EPSG:2180 columns
    /// hold what the parser would have put there in each case.
    fn raw_table(conn: &Connection, n: i64, easting: f64, northing: f64, mirrored: bool) {
        let (x, y) = if mirrored {
            (northing, easting)
        } else {
            (easting, northing)
        };
        conn.execute_batch(&format!(
            "CREATE OR REPLACE TABLE prg_raw AS
             SELECT 'id-' || i AS lokalny_id, x AS x_epsg_2180, y AS y_epsg_2180,
                    ST_X(p) AS dlugosc_geograficzna, ST_Y(p) AS szerokosc_geograficzna
             FROM (SELECT i, x, y,
                          ST_Transform(ST_Point(x, y), 'EPSG:2180', 'EPSG:4326',
                                       always_xy := true) AS p
                   FROM (SELECT i, {x} + i * 10 AS x, {y} + i * 10 AS y
                         FROM range({n}) t(i)))"
        ))
        .unwrap();
    }

    /// Around Hrubieszów (23.9°E, 50.8°N): easting above Poland's largest
    /// northing, so its mirror lands north of the country.
    const EAST: (f64, f64) = (850_000.0, 330_000.0);
    /// Lubuskie, where the checked-in PRG fixture lives.
    const LUBUSKIE: (f64, f64) = (238_447.0, 516_743.0);

    fn error_of(conn: &Connection) -> String {
        format!(
            "{:#}",
            check_axis_order(conn, "prg_raw", ALL_ROWS).unwrap_err()
        )
    }

    #[test]
    fn eastern_rows_pass_as_read_and_fail_mirrored() {
        let conn = conn();
        raw_table(&conn, 100, EAST.0, EAST.1, false);
        check_axis_order(&conn, "prg_raw", ALL_ROWS).unwrap();

        raw_table(&conn, 100, EAST.0, EAST.1, true);
        let err = error_of(&conn);
        assert!(err.starts_with(AXIS_ORDER_BAIL_MESSAGE), "{err}");
        assert!(err.ends_with(EXCHANGED_FITS_HINT), "{err}");
        assert!(err.contains("100 of 100 rows"), "{err}");
        assert!(err.contains("id-"), "{err}");
    }

    /// Reversed `ST_Point` arguments: latitude where longitude belongs. The
    /// EPSG:2180 pair is still right, so exchanging *it* mirrors the rows out
    /// of the box and neither reading fits.
    #[test]
    fn lon_and_lat_exchanged_fits_neither_way() {
        let conn = conn();
        raw_table(&conn, 100, EAST.0, EAST.1, false);
        conn.execute_batch(
            "UPDATE prg_raw SET dlugosc_geograficzna = szerokosc_geograficzna,
                                szerokosc_geograficzna = dlugosc_geograficzna",
        )
        .unwrap();

        let err = error_of(&conn);
        assert!(err.starts_with(AXIS_ORDER_BAIL_MESSAGE), "{err}");
        assert!(err.contains(NEITHER_FITS_HINT), "{err}");
        assert!(!err.contains(EXCHANGED_FITS_HINT), "{err}");
    }

    /// 1 stray row in 1,000 is exactly `MAX_OUTSIDE_SHARE` and passes; a
    /// second one is over it.
    #[test]
    fn the_tolerance_is_inclusive() {
        let conn = conn();
        raw_table(&conn, 1000, EAST.0, EAST.1, false);
        let stray = |ids: &str| {
            conn.execute_batch(&format!(
                "UPDATE prg_raw SET dlugosc_geograficzna = 0.0, szerokosc_geograficzna = 0.0
                 WHERE lokalny_id IN ({ids})"
            ))
            .unwrap();
        };

        stray("'id-0'");
        check_axis_order(&conn, "prg_raw", ALL_ROWS).unwrap();

        stray("'id-1'");
        let err = error_of(&conn);
        assert!(err.starts_with(AXIS_ORDER_BAIL_MESSAGE), "{err}");
        assert!(err.contains("2 of 1000 rows"), "{err}");
    }

    /// The check judges only the rows the load select will keep.
    #[test]
    fn rows_outside_the_row_filter_are_not_judged() {
        let conn = conn();
        raw_table(&conn, 10, EAST.0, EAST.1, false);
        conn.execute_batch(
            "UPDATE prg_raw SET dlugosc_geograficzna = 0.0, szerokosc_geograficzna = 0.0
             WHERE lokalny_id = 'id-0'",
        )
        .unwrap();

        assert!(check_axis_order(&conn, "prg_raw", ALL_ROWS).is_err());
        check_axis_order(&conn, "prg_raw", "lokalny_id <> 'id-0'").unwrap();
    }

    /// A raw table without the EPSG:2180 columns still gets the real error,
    /// not a "column not found" from the query that only chooses the hint.
    #[test]
    fn a_missing_epsg_2180_pair_does_not_mask_the_error() {
        let conn = conn();
        raw_table(&conn, 10, EAST.0, EAST.1, true);
        conn.execute_batch(
            "ALTER TABLE prg_raw DROP COLUMN x_epsg_2180;
             ALTER TABLE prg_raw DROP COLUMN y_epsg_2180;",
        )
        .unwrap();

        let err = error_of(&conn);
        assert!(err.starts_with(AXIS_ORDER_BAIL_MESSAGE), "{err}");
        assert!(err.ends_with(NEITHER_FITS_HINT), "{err}");
    }

    /// **This pins a limitation, not a feature.** Lubuskie mirrors onto Polish
    /// territory, so a mirrored load of it alone passes -- which is why the
    /// checked-in lubuskie fixture cannot exercise the failing path, and why
    /// nobody should read this check as proof that a partial load is right.
    /// If this test ever fails, the check got stronger: update
    /// `POLAND_EXTENT`'s doc comment and CLAUDE.md, not this assertion alone.
    #[test]
    fn a_mirrored_load_of_lubuskie_alone_passes() {
        let conn = conn();
        raw_table(&conn, 100, LUBUSKIE.0, LUBUSKIE.1, true);
        check_axis_order(&conn, "prg_raw", ALL_ROWS).unwrap();
    }

    #[test]
    fn an_empty_load_is_not_this_check_s_business() {
        let conn = conn();
        raw_table(&conn, 0, EAST.0, EAST.1, false);
        check_axis_order(&conn, "prg_raw", ALL_ROWS).unwrap();
    }
}
