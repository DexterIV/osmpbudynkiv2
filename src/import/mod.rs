pub mod bdot10k;
pub mod egib;
pub mod osm;
pub mod prg;

use anyhow::Result;
use duckdb::Connection;
use tracing::{info, warn};

use crate::cli::ImportSource;
use crate::config::{Config, DownloadUrls};
use crate::osm::kvstore::RocksDB;

pub fn run(
    conn: &Connection,
    kv: &RocksDB,
    source: ImportSource,
    config: &Config,
    urls: &DownloadUrls,
) -> Result<()> {
    match source {
        ImportSource::Osm { file } => osm::import(conn, kv, config, file.as_deref(), &urls.osm_pbf),
        ImportSource::Bdot10k { file } => {
            bdot10k::import(conn, config, file.as_deref(), &urls.bdot10k)?;
            reconcile_reports(conn, &crate::dataset::BDOT10K);
            Ok(())
        }
        ImportSource::Egib { file } => {
            egib::import(conn, config, file.as_deref(), &urls.egib)?;
            reconcile_reports(conn, &crate::dataset::EGIB);
            Ok(())
        }
        ImportSource::Prg { file, terc_file } => {
            prg::import(
                conn,
                config,
                file.as_deref(),
                terc_file.as_deref(),
                &urls.prg,
            )?;
            reconcile_reports(conn, &crate::dataset::PRG);
            Ok(())
        }
        ImportSource::StreetMappings { file, url } => {
            run_street_mappings_import(conn, config, file, url.as_deref(), &urls.street_mappings)
        }
        ImportSource::BuildingTypes {
            bdot10k_file,
            egib_file,
            bdot10k_url,
            egib_url,
        } => run_building_types_import(
            conn,
            config,
            (
                bdot10k_file,
                bdot10k_url.as_deref(),
                &urls.bdot10k_building_types,
            ),
            (egib_file, egib_url.as_deref(), &urls.egib_building_types),
        ),
        ImportSource::Full {
            osm_file,
            bdot10k_file,
            egib_file,
            prg_file,
            terc_file,
            street_mappings_file,
            bdot10k_building_types_file,
            egib_building_types_file,
        } => {
            osm::import(conn, kv, config, osm_file.as_deref(), &urls.osm_pbf)?;
            crate::shutdown::check_requested()?;
            bdot10k::import(conn, config, bdot10k_file.as_deref(), &urls.bdot10k)?;
            crate::shutdown::check_requested()?;
            egib::import(conn, config, egib_file.as_deref(), &urls.egib)?;
            crate::shutdown::check_requested()?;
            prg::import(
                conn,
                config,
                prg_file.as_deref(),
                terc_file.as_deref(),
                &urls.prg,
            )?;
            crate::shutdown::check_requested()?;
            run_street_mappings_import(
                conn,
                config,
                street_mappings_file,
                None,
                &urls.street_mappings,
            )?;
            crate::shutdown::check_requested()?;
            run_building_types_import(
                conn,
                config,
                (
                    bdot10k_building_types_file,
                    None,
                    &urls.bdot10k_building_types,
                ),
                (egib_building_types_file, None, &urls.egib_building_types),
            )?;
            // All three government sources were rebuilt wholesale, so every
            // source's reports need re-checking -- see `reconcile_reports`.
            // OSM is not a reported source and has no entry here.
            for spec in [
                &crate::dataset::BDOT10K,
                &crate::dataset::EGIB,
                &crate::dataset::PRG,
            ] {
                reconcile_reports(conn, spec);
            }
            Ok(())
        }
    }
}

/// Resolve `file` vs. downloading from `url`/`default_url`, load the street-name
/// mappings through `mappings::load_from_path`, clean up a downloaded file per
/// `config.cleanup_downloaded_files`, and record the outcome to `job_run_log`.
/// Shared by the standalone `import street-mappings` command and `import full`.
fn run_street_mappings_import(
    conn: &Connection,
    config: &Config,
    file: Option<std::path::PathBuf>,
    url: Option<&str>,
    default_url: &str,
) -> Result<()> {
    let outcome = (|| -> Result<crate::mappings::LoadStats> {
        let (path, was_downloaded) = match file {
            Some(p) => (p, false),
            None => {
                let src = url.unwrap_or(default_url);
                let downloaded = crate::download::download_file_as(
                    src,
                    &config.download_dir(),
                    "street_names_mappings.csv",
                )?;
                (downloaded, true)
            }
        };
        let stats = crate::mappings::load_from_path(conn, &path)?;

        if was_downloaded {
            if config.cleanup_downloaded_files {
                info!(path = %path.display(), "Cleaning up downloaded file");
                let _ = std::fs::remove_file(&path);
            } else {
                warn!(
                    path = %path.display(),
                    "cleanup_downloaded_files is false; leaving downloaded file in place \
                     (it will be reused on the next run since download_file_as skips \
                     re-downloading an existing destination)"
                );
            }
        }

        Ok(stats)
    })();
    match &outcome {
        Ok(stats) => {
            let msg = format!(
                "loaded {} mapping rows ({} not present in current PRG data, \
                 {} dirty cell(s) enqueued)",
                stats.rows_loaded, stats.rows_absent_from_prg, stats.cells_enqueued
            );
            let _ = crate::job_log::record(conn, "import:street-mappings", "Success", Some(&msg));
        }
        Err(e) => {
            let _ = crate::job_log::record(
                conn,
                "import:street-mappings",
                "Error",
                Some(&format!("{e:#}")),
            );
        }
    }
    outcome.map(|_| ())
}

/// `(file, url_override, default_url)` for one building-type mapping source.
type BuildingTypesInput<'a> = (Option<std::path::PathBuf>, Option<&'a str>, &'a str);

/// Resolve and load both building-type mapping CSVs (mirrors
/// `run_street_mappings_import`), recording one combined `job_run_log` entry.
/// Shared by the standalone `import building-types` command and `import full`.
fn run_building_types_import(
    conn: &Connection,
    config: &Config,
    bdot10k: BuildingTypesInput,
    egib: BuildingTypesInput,
) -> Result<()> {
    use crate::mappings::building_types::{BDOT10K, BuildingTypeStats, EGIB};

    let outcome = (|| -> Result<(BuildingTypeStats, BuildingTypeStats)> {
        let (bdot10k_file, bdot10k_url, default_bdot10k_url) = bdot10k;
        let (egib_file, egib_url, default_egib_url) = egib;
        let bdot10k_stats = load_building_type_file(
            conn,
            config,
            &BDOT10K,
            bdot10k_file,
            bdot10k_url.unwrap_or(default_bdot10k_url),
            "bdot10k_building_types.csv",
        )?;
        let egib_stats = load_building_type_file(
            conn,
            config,
            &EGIB,
            egib_file,
            egib_url.unwrap_or(default_egib_url),
            "egib_building_types.csv",
        )?;
        Ok((bdot10k_stats, egib_stats))
    })();

    match &outcome {
        Ok((b, e)) => {
            let msg = format!(
                "bdot10k: loaded {} rows ({} keys absent from source, {} source keys \
                 / {} source rows uncovered); egib: loaded {} rows ({} keys absent \
                 from source, {} source keys / {} source rows uncovered)",
                b.rows_loaded,
                b.keys_absent_from_source,
                b.source_keys_uncovered,
                b.source_rows_uncovered,
                e.rows_loaded,
                e.keys_absent_from_source,
                e.source_keys_uncovered,
                e.source_rows_uncovered,
            );
            let _ = crate::job_log::record(conn, "import:building-types", "Success", Some(&msg));
        }
        Err(e) => {
            let _ = crate::job_log::record(
                conn,
                "import:building-types",
                "Error",
                Some(&format!("{e:#}")),
            );
        }
    }
    outcome.map(|_| ())
}

/// Resolve `file` vs. downloading from `url` (mirrors the `StreetMappings`
/// arm above), load it through `source`, then clean up a downloaded file per
/// `config.cleanup_downloaded_files` -- a user-supplied `--*-file` is never
/// deleted regardless of that setting.
fn load_building_type_file(
    conn: &Connection,
    config: &Config,
    source: &crate::mappings::building_types::BuildingTypeSource,
    file: Option<std::path::PathBuf>,
    url: &str,
    download_filename: &str,
) -> Result<crate::mappings::building_types::BuildingTypeStats> {
    let (path, was_downloaded) = match file {
        Some(p) => (p, false),
        None => {
            let downloaded =
                crate::download::download_file_as(url, &config.download_dir(), download_filename)?;
            (downloaded, true)
        }
    };
    let stats = crate::mappings::building_types::load_from_path(conn, source, &path)?;

    if was_downloaded {
        if config.cleanup_downloaded_files {
            info!(path = %path.display(), "Cleaning up downloaded file");
            let _ = std::fs::remove_file(&path);
        } else {
            warn!(
                path = %path.display(),
                "cleanup_downloaded_files is false; leaving downloaded file in place \
                 (it will be reused on the next run since download_file_as skips \
                 re-downloading an existing destination)"
            );
        }
    }
    Ok(stats)
}

/// `metadata` key holding when `import <source>` last finished successfully,
/// as `now()::VARCHAR` (read back with `TRY_CAST(value AS TIMESTAMPTZ)`).
///
/// Imports need their own stamp because they leave no other durable one:
/// `dataset_refreshes` is written only by `update` (its readers assume every
/// row is a diff), and `job_run_log` keeps only the latest run -- a failed
/// import erases the last successful one, and `import prg` writes no row at
/// all. "Last successful load" of a source is therefore the later of this and
/// `max(dataset_refreshes.finished_at)`.
pub fn imported_at_key(source: &str) -> String {
    format!("imported_at:{source}")
}

/// Stamp `imported_at_key(spec.name)` with the current time. Called by each
/// government loader as its last step, so only a fully successful import
/// (table loaded *and* indexed) is stamped.
///
/// Best-effort, for the same reason as `reconcile_reports` below: the import
/// has already landed, and failing it over a freshness stamp would make the
/// operator redo tens of minutes of work for a cosmetic value.
pub(crate) fn record_imported_at(conn: &Connection, spec: &crate::dataset::DatasetSpec) {
    let key = imported_at_key(spec.name);
    let outcome = crate::compare::in_transaction(conn, "record import time", || {
        conn.execute("DELETE FROM metadata WHERE key = ?", duckdb::params![key])?;
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?, now()::VARCHAR)",
            duckdb::params![key],
        )?;
        Ok(())
    });
    if let Err(e) = outcome {
        warn!(source = spec.name, error = %e, "could not record the import time");
    }
}

/// Retire user reports whose records changed across a re-import.
///
/// **This is the only thing that catches that case.** A refresh detects change
/// via `update::diff`, and `update::dataset::refresh` retires reports inside
/// its apply transaction on the strength of it. An `import` has no diff at all
/// — it rebuilds the table wholesale — so without this call a report submitted
/// before a re-import would keep vetoing its object forever, even though the
/// record it described was replaced. The content signature is what makes this
/// possible without a second copy of the old data to compare against.
///
/// Best-effort: a reconcile failure must not fail an otherwise-successful
/// import, because the import's own work has already landed and the standing
/// safety nets (`reports reconcile`, the background job) re-cover it. Warn
/// rather than propagate, the same call `package::log_export` makes.
fn reconcile_reports(conn: &Connection, spec: &crate::dataset::DatasetSpec) {
    match crate::reports::reconcile_source(conn, spec) {
        Ok(stats) if stats.total_expired() > 0 => info!(
            source = spec.name,
            changed = stats.expired_changed,
            removed = stats.expired_removed,
            "retired user reports whose records moved on"
        ),
        Ok(_) => {}
        Err(e) => warn!(
            source = spec.name,
            error = %e,
            "could not reconcile user reports after import; run `reports reconcile`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let init = vec!["INSTALL spatial".to_string(), "LOAD spatial".to_string()];
        crate::db::init_db(std::path::Path::new(":memory:"), &init, None).unwrap()
    }

    fn stamps(conn: &Connection, source: &str) -> Vec<Option<String>> {
        let mut stmt = conn
            .prepare("SELECT TRY_CAST(value AS TIMESTAMPTZ)::VARCHAR FROM metadata WHERE key = ?")
            .unwrap();
        stmt.query_map(duckdb::params![imported_at_key(source)], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// `metadata` has no key constraint, so the stamp relies on its own
    /// delete-then-insert to stay a single row -- and readers cast it back to
    /// a timestamp, so it must parse as one.
    #[test]
    fn a_repeated_import_leaves_one_parseable_stamp_per_source() {
        let conn = conn();
        record_imported_at(&conn, &crate::dataset::PRG);
        record_imported_at(&conn, &crate::dataset::PRG);
        record_imported_at(&conn, &crate::dataset::EGIB);

        let prg = stamps(&conn, "prg");
        assert_eq!(prg.len(), 1);
        assert!(prg[0].is_some(), "stamp must parse as TIMESTAMPTZ");
        assert_eq!(stamps(&conn, "egib").len(), 1);
        assert!(stamps(&conn, "bdot10k").is_empty());
    }
}
