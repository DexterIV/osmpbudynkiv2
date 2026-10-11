//! The slice of the server's configuration `/status` publishes.
//!
//! The frontend is served from disk, separately from the binary, and used to
//! restate three config values as constants of its own (`/package`'s area
//! cap, `POST /report`'s object cap, the change overlay's reach). Those could
//! only ever match the *defaults*, so a deployment that changed one got a
//! frontend that blocked requests the server would accept or sent ones it was
//! guaranteed to reject. Publishing the values lets the frontend follow the
//! server it is actually talking to; its own constants remain as the fallback
//! for a `/status` that failed.
//!
//! **This is an allowlist, and it has to stay one.** `PublicConfig` is filled
//! field by field in `from_config` and `Config` itself deliberately does not
//! derive `Serialize`: a "serialize everything except the secrets" shape
//! publishes every field added later by default, and `Config` holds the TERYT
//! API password, filesystem paths, the listen address and the backup
//! directory. `the_published_config_has_exactly_these_fields` pins the key
//! set, so widening it is a deliberate edit rather than a side effect.

use serde::Serialize;

use crate::config::Config;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PublicConfig {
    pub package: PublicPackageConfig,
    pub reports: PublicReportsConfig,
    pub changes: PublicChangesConfig,
    pub download_urls: PublicDownloadUrls,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PublicPackageConfig {
    pub max_area_sq_deg: f64,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PublicReportsConfig {
    pub enabled: bool,
    pub max_objects_per_request: usize,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PublicChangesConfig {
    pub max_age_days: u64,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PublicDownloadUrls {
    /// `null` when the configured value is not a plain http(s) URL -- see
    /// `public_url`.
    pub osm_replication: Option<String>,
}

impl PublicConfig {
    /// The nesting mirrors the TOML, so a published value is found under the
    /// same path an operator sets it at.
    pub fn from_config(config: &Config) -> Self {
        Self {
            package: PublicPackageConfig {
                max_area_sq_deg: config.package.max_area_sq_deg,
            },
            reports: PublicReportsConfig {
                enabled: config.reports.enabled,
                max_objects_per_request: config.reports.max_objects_per_request,
            },
            changes: PublicChangesConfig {
                max_age_days: config.changes.max_age_days,
            },
            download_urls: PublicDownloadUrls {
                osm_replication: public_url(&config.download_urls.osm_replication),
            },
        }
    }
}

/// An operator-supplied URL in the form that is safe to publish: http(s)
/// only, with userinfo, query string and fragment removed.
///
/// The default feeds are public, but nothing stops a deployment pointing at a
/// private mirror with credentials in the URL, and those travel as userinfo
/// (`https://user:pass@host/`) or in the query (`?token=`). Anything that is
/// not an http(s) URL -- a `file://` URL, a bare path -- is withheld entirely
/// rather than published as a description of the host's filesystem.
///
/// A secret carried in the *path* cannot be told apart from an ordinary path
/// segment and is published as-is; `example_config.toml` says so next to the
/// setting.
fn public_url(raw: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    // Both setters fail only for a URL that cannot carry userinfo at all (no
    // host), which an http(s) URL that parsed always can. Withhold rather
    // than publish if that ever stops holding.
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_url_is_published_unchanged() {
        assert_eq!(
            public_url("https://download.openstreetmap.fr/replication/europe/poland/minute")
                .as_deref(),
            Some("https://download.openstreetmap.fr/replication/europe/poland/minute")
        );
    }

    #[test]
    fn credentials_in_a_url_are_stripped() {
        assert_eq!(
            public_url("https://alice:hunter2@mirror.example/replication/minute?token=s3cret#frag")
                .as_deref(),
            Some("https://mirror.example/replication/minute")
        );
        // A username alone is still a credential.
        assert_eq!(
            public_url("https://alice@mirror.example/minute").as_deref(),
            Some("https://mirror.example/minute")
        );
    }

    #[test]
    fn anything_but_an_http_url_is_withheld() {
        assert_eq!(public_url("file:///srv/osm/replication"), None);
        assert_eq!(public_url("/srv/osm/replication"), None);
        assert_eq!(public_url("./replication"), None);
        assert_eq!(public_url(""), None);
    }

    #[test]
    fn the_published_values_follow_the_config() {
        let mut config = Config::default();
        config.package.max_area_sq_deg = 0.125;
        config.reports.enabled = false;
        config.reports.max_objects_per_request = 25;
        config.changes.max_age_days = 3;
        config.download_urls.osm_replication = "https://mirror.example/minute".to_string();

        let v = serde_json::to_value(PublicConfig::from_config(&config)).unwrap();
        assert_eq!(v["package"]["max_area_sq_deg"], 0.125);
        assert_eq!(v["reports"]["enabled"], false);
        assert_eq!(v["reports"]["max_objects_per_request"], 25);
        assert_eq!(v["changes"]["max_age_days"], 3);
        assert_eq!(
            v["download_urls"]["osm_replication"],
            "https://mirror.example/minute"
        );
    }

    /// The allowlist, pinned. A field added to `PublicConfig` fails this, which
    /// is the point: the question "is this safe to hand to an anonymous
    /// client" gets asked at the moment the answer changes.
    #[test]
    fn the_published_config_has_exactly_these_fields() {
        let v = serde_json::to_value(PublicConfig::from_config(&Config::default())).unwrap();
        let mut paths = Vec::new();
        for (section, fields) in v.as_object().unwrap() {
            for field in fields.as_object().unwrap().keys() {
                paths.push(format!("{section}.{field}"));
            }
        }
        paths.sort();
        assert_eq!(
            paths,
            [
                "changes.max_age_days",
                "download_urls.osm_replication",
                "package.max_area_sq_deg",
                "reports.enabled",
                "reports.max_objects_per_request",
            ]
        );
    }

    /// Fills every secret-shaped setting with a marker and checks none of them
    /// reaches the published JSON, whichever field it would have to leak
    /// through.
    #[test]
    fn nothing_sensitive_reaches_the_published_config() {
        const MARKER: &str = "LEAKED";
        let mut config = Config {
            db_path: format!("/srv/{MARKER}/db.duckdb"),
            rocksdb_path: format!("/srv/{MARKER}/kv"),
            web_dir: format!("/srv/{MARKER}/web"),
            download_dir: Some(format!("/srv/{MARKER}/downloads")),
            http_listen_addr: format!("{MARKER}:3000"),
            duckdb_init_commands: vec![format!("SET GLOBAL temp_directory = '/{MARKER}'")],
            ..Config::default()
        };
        config.teryt.api_username = Some(MARKER.to_string());
        config.teryt.api_password = Some(MARKER.to_string());
        config.teryt.file_path = Some(format!("/srv/{MARKER}/teryt.csv"));
        config.jobs.backup.dir = format!("/srv/{MARKER}/backups");
        config.download_urls.osm_replication =
            format!("https://{MARKER}:{MARKER}@mirror.example/minute?key={MARKER}");

        let json = serde_json::to_string(&PublicConfig::from_config(&config)).unwrap();
        assert!(!json.contains(MARKER), "published config leaked: {json}");
    }
}
