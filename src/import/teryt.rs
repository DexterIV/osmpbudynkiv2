//! `teryt_units`: the TERC catalogue as a table -- every województwo, powiat
//! and gmina with its official name.
//!
//! Its purpose is the context PRG does not store. `prg_addresses` carries
//! only `teryt_gmina`/`gmina` (the loader projects the powiat and województwo
//! away), and 226 gmina names belong to more than one gmina, so the name alone
//! cannot tell them apart. The powiat and województwo of a PRG row are the
//! 4- and 2-digit prefixes of its `teryt_gmina`, which is how this table is
//! meant to be joined -- by prefix, never by the full 7-digit code. A gmina's
//! last digit is its type and changes when, say, a village gets town rights
//! (2 -> 3), so a TERC fetched today and a PRG snapshot published earlier can
//! disagree on it while agreeing on both prefixes.
//!
//! **Written only by a PRG load, from the TERC map that load already holds**
//! (`import prg` and a refresh that actually ran; an ETag-unchanged refresh
//! downloads neither PRG nor TERC). There is deliberately no separate job:
//! TERC changes about once a year, and tying it to the PRG load means both
//! describe the same fetch. A database loaded before this table existed has
//! it empty until the next such load, so a reader must treat a missing row as
//! "no context", never as an error.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use duckdb::Connection;
use prg_convert::terc::Terc;

use crate::db::values_sql;

pub const LEVEL_WOJEWODZTWO: &str = "wojewodztwo";
pub const LEVEL_POWIAT: &str = "powiat";
/// A 7-digit TERC code of one of [`GMINA_TYPES`].
pub const LEVEL_GMINA: &str = "gmina";

/// The 7-digit TERC types kept: gmina miejska, wiejska, miejsko-wiejska. The
/// type is the code's last digit. TERC also lists the urban and rural parts of
/// a mixed gmina (4, 5) and the districts and delegatury of the large cities
/// (8, 9), but no PRG address references them -- measured on the national
/// snapshot, 2026-09-28: every `teryt_gmina` ends in 1, 2 or 3 -- so they are
/// dropped rather than offered as units nothing can be found in.
pub const GMINA_TYPES: [char; 3] = ['1', '2', '3'];

/// Rows per `INSERT`. The whole table is ~2,900 rows, three columns each.
const INSERT_CHUNK: usize = 1000;

/// One row of `teryt_units`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerytUnit {
    pub code: String,
    pub level: &'static str,
    pub name: String,
}

/// Flatten `prg_convert`'s map into one row per unit, sorted by code,
/// keeping only the gminy of [`GMINA_TYPES`].
///
/// The map is keyed by 7-digit code and repeats its województwo and powiat on
/// every entry; those are de-duplicated by code, and taken only from kept
/// gminy. Nothing is lost by that: every powiat has at least one gmina of a
/// kept type (`the_catalogue_keeps_every_gmina_powiat_and_wojewodztwo`
/// checks the counts against the fixture's raw rows). Note `prg_convert`
/// lowercases the województwo names, which TERC publishes in capitals.
pub fn units_from_terc(terc: &HashMap<String, Terc>) -> Vec<TerytUnit> {
    let mut units: BTreeMap<String, (&'static str, String)> = BTreeMap::new();
    for (code, t) in terc {
        if !code.ends_with(GMINA_TYPES) {
            continue;
        }
        units
            .entry(t.voivodeship_teryt_id.clone())
            .or_insert_with(|| (LEVEL_WOJEWODZTWO, t.voivodeship_name.clone()));
        units
            .entry(t.county_teryt_id.clone())
            .or_insert_with(|| (LEVEL_POWIAT, t.county_name.clone()));
        units.insert(code.clone(), (LEVEL_GMINA, t.municipality_name.clone()));
    }
    units
        .into_iter()
        .map(|(code, (level, name))| TerytUnit { code, level, name })
        .collect()
}

/// Replace the contents of `teryt_units` with `terc`, in one transaction, so
/// a reader never sees the table half-written. Returns the row count.
pub fn store(conn: &Connection, terc: &HashMap<String, Terc>) -> Result<usize> {
    let units = units_from_terc(terc);
    crate::compare::in_transaction(conn, "store teryt_units", || {
        conn.execute_batch("DELETE FROM teryt_units")
            .context("Failed to clear teryt_units")?;
        for chunk in units.chunks(INSERT_CHUNK) {
            let params = chunk
                .iter()
                .flat_map(|u| [u.code.as_str(), u.level, u.name.as_str()]);
            conn.execute(
                &format!(
                    "INSERT INTO teryt_units (code, level, name) VALUES {}",
                    values_sql(chunk.len(), &["VARCHAR", "VARCHAR", "VARCHAR"])
                ),
                duckdb::params_from_iter(params),
            )
            .context("Failed to insert into teryt_units")?;
        }
        Ok(units.len())
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::db::init_db;

    fn fixture_terc() -> HashMap<String, Terc> {
        prg_convert::get_teryt_mapping(
            false,
            &None,
            &None,
            &Some(PathBuf::from("fixtures/teryt.zip")),
        )
        .unwrap()
    }

    fn conn() -> Connection {
        let init = ["INSTALL spatial".to_string(), "LOAD spatial".to_string()];
        init_db(Path::new(":memory:"), &init, None).unwrap()
    }

    fn name_of(conn: &Connection, code: &str) -> (String, String) {
        conn.query_row(
            "SELECT level, name FROM teryt_units WHERE code = ?",
            [code],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    /// `fixtures/teryt.zip` is the full national TERC of 2025-11-18: 16
    /// województwa, 380 powiaty and 3,952 7-digit units, 2,479 of them of a
    /// kept type (302 + 1,459 + 718). Arriving at exactly those counts through
    /// the per-gmina map is what shows the flattening drops no powiat.
    #[test]
    fn the_catalogue_keeps_every_gmina_powiat_and_wojewodztwo() {
        let conn = conn();
        assert_eq!(store(&conn, &fixture_terc()).unwrap(), 2875);

        let mut stmt = conn
            .prepare("SELECT level, count(*) FROM teryt_units GROUP BY level ORDER BY level")
            .unwrap();
        let counts: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<duckdb::Result<_>>()
            .unwrap();
        assert_eq!(
            counts,
            [
                (LEVEL_GMINA.to_string(), 2479),
                (LEVEL_POWIAT.to_string(), 380),
                (LEVEL_WOJEWODZTWO.to_string(), 16),
            ]
        );

        let bad_lengths: i64 = conn
            .query_row(
                "SELECT count(*) FROM teryt_units
                 WHERE length(code) <> CASE level WHEN 'wojewodztwo' THEN 2
                                                  WHEN 'powiat' THEN 4
                                                  ELSE 7 END",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad_lengths, 0);
    }

    #[test]
    fn each_level_keeps_its_own_name() {
        let conn = conn();
        store(&conn, &fixture_terc()).unwrap();
        // Three units named after one town, told apart only by level and code.
        assert_eq!(
            name_of(&conn, "02"),
            ("wojewodztwo".into(), "dolnośląskie".into())
        );
        assert_eq!(
            name_of(&conn, "0201"),
            ("powiat".into(), "bolesławiecki".into())
        );
        assert_eq!(
            name_of(&conn, "0201011"),
            ("gmina".into(), "Bolesławiec".into())
        );
        assert_eq!(
            name_of(&conn, "0201022"),
            ("gmina".into(), "Bolesławiec".into())
        );
    }

    #[test]
    fn units_prg_never_references_are_dropped() {
        let conn = conn();
        store(&conn, &fixture_terc()).unwrap();
        let other_types: i64 = conn
            .query_row(
                "SELECT count(*) FROM teryt_units
                 WHERE level = 'gmina' AND right(code, 1) NOT IN ('1', '2', '3')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(other_types, 0);
        // Named examples of each dropped kind: a Warsaw district (8), and the
        // urban (4) and rural (5) parts of the mixed gmina Bełżyce.
        let dropped: i64 = conn
            .query_row(
                "SELECT count(*) FROM teryt_units
                 WHERE code IN ('1465028', '0609014', '0609015')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dropped, 0);
        assert_eq!(
            name_of(&conn, "0609013"),
            ("gmina".into(), "Bełżyce".into())
        );
    }

    #[test]
    fn storing_again_replaces_rather_than_appends() {
        let conn = conn();
        let terc = fixture_terc();
        store(&conn, &terc).unwrap();
        conn.execute_batch("INSERT INTO teryt_units VALUES ('99', 'wojewodztwo', 'stale')")
            .unwrap();
        store(&conn, &terc).unwrap();

        let (rows, stale): (i64, i64) = conn
            .query_row(
                "SELECT count(*), count(*) FILTER (WHERE code = '99') FROM teryt_units",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, stale), (2875, 0));
    }
}
