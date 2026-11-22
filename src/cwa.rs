//! Automatic mappings for books delivered by Calibre-Web Automated's Kobo API.
//!
//! Kobo stores a CWA book under the UUID returned by `/v1/library/sync` and
//! keeps the CWA endpoint, including its token, in `Kobo eReader.conf`. That
//! gives the live plugin enough information to fetch the original EPUB when a
//! new kepub is opened. The original stays in NickelInkSync's private device
//! directory; it is never copied to the desktop.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use ureq::Agent;

use crate::db::{Book, Device};
use crate::hash::partial_md5;
use crate::mapping::{DeviceBook, Mapping, Original};
use crate::prepare::{self, Prepared};
use crate::sync::iso_timestamp;

const CONF: &str = ".kobo/Kobo/Kobo eReader.conf";
const ORIGINALS: &str = "originals";

#[derive(Debug)]
enum Error {
  Http,
  Status(u16),
  Response(String),
  Io(PathBuf, io::Error),
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      // The native Kobo URL contains the CWA sync token. Never include the
      // request URL in a message that may end up in NickelInkSync.log.
      Error::Http => write!(f, "CWA request failed"),
      Error::Status(status) => write!(f, "CWA returned HTTP {status}"),
      Error::Response(e) => write!(f, "CWA returned unexpected data: {e}"),
      Error::Io(path, e) => write!(f, "{}: {e}", path.display()),
    }
  }
}

impl std::error::Error for Error {}

impl From<ureq::Error> for Error {
  fn from(_: ureq::Error) -> Self {
    Error::Http
  }
}

#[derive(Debug, Deserialize)]
struct Metadata {
  #[serde(rename = "DownloadUrls", default)]
  download_urls: Vec<DownloadUrl>,
}

#[derive(Debug, Deserialize)]
struct DownloadUrl {
  #[serde(rename = "Url")]
  url: String,
}

/// Maps the selected CWA books on a live Kobo. A missing or non-CWA endpoint
/// is normal and does nothing. A failed download is also left for the next
/// run, so a temporary network error does not stop Nickel opening a book.
pub fn auto_map(device: &Device, books: &[Book], timeout: Duration) -> Result<(), String> {
  let Some(endpoint) = api_endpoint(&device.root) else {
    return Ok(());
  };
  let agent = Agent::config_builder()
    .http_status_as_error(false)
    .timeout_global(Some(timeout.max(Duration::from_secs(1))))
    .user_agent(concat!("nickelinksync/", env!("CARGO_PKG_VERSION")))
    .build()
    .into();
  let prepared = Prepared::load(&device.root).map_err(|e| format!("prepared files: {e}"))?;

  for book in books {
    if !book.is_kepub() || !is_uuid(&book.content_id) {
      continue;
    }
    let file = device.book_file(book);
    let Ok(hash) = partial_md5(&file) else {
      continue;
    };
    if prepared
      .as_ref()
      .and_then(|p| p.mappings.for_file(&hash))
      .is_some()
    {
      continue;
    }
    // A CWA book that is no longer in the user's library returns 404. It is
    // not an error for a Kobo to contain other UUID-based books, so retrying
    // on a later run is the only action needed.
    if let Err(error) = map_book(device, book, &hash, &endpoint, &agent) {
      eprintln!("nickelinksync: CWA auto-mapping skipped: {error}");
    }
  }
  Ok(())
}

fn map_book(
  device: &Device,
  book: &Book,
  device_hash: &str,
  endpoint: &str,
  agent: &Agent,
) -> Result<(), Error> {
  let originals = device.root.join(prepare::DIR).join(ORIGINALS);
  fs::create_dir_all(&originals).map_err(|e| Error::Io(originals.clone(), e))?;
  let original_path = originals.join(format!("{}.epub", cache_name(&book.content_id)));
  if !original_path.is_file() {
    let metadata = metadata(agent, endpoint, &book.content_id)?;
    let url = metadata
      .first()
      .and_then(|book| book.download_urls.first())
      .map(|download| original_url(&download.url))
      .transpose()?
      .ok_or_else(|| Error::Response("metadata has no download URL".into()))?;
    download(agent, &url, &original_path)?;
  }
  let original_hash = partial_md5(&original_path)
    .map_err(|e| Error::Io(original_path.clone(), io::Error::other(e.to_string())))?;
  let mapping = Mapping {
    device: DeviceBook::File {
      hash: device_hash.to_string(),
      path: book.storage_path(),
      title: book.title.clone(),
      author: book.author.clone(),
    },
    epub: Original {
      path: original_path.to_string_lossy().into_owned(),
      hash: original_hash,
      title: book.title.clone(),
      author: book.author.clone(),
    },
    created: iso_timestamp(crate::history::now()),
  };
  prepare::put_live_mapping(&device.root, &mapping)
    .map_err(|error| Error::Response(error.to_string()))
}

fn metadata(agent: &Agent, endpoint: &str, content_id: &str) -> Result<Vec<Metadata>, Error> {
  let response = agent
    .get(format!("{endpoint}/v1/library/{content_id}/metadata"))
    .header("accept", "application/json")
    .call()?;
  let status = response.status().as_u16();
  if !(200..=299).contains(&status) {
    return Err(Error::Status(status));
  }
  response
    .into_body()
    .read_json()
    .map_err(|e| Error::Response(e.to_string()))
}

fn download(agent: &Agent, url: &str, path: &Path) -> Result<(), Error> {
  let mut response = agent
    .get(url)
    .header("accept", "application/epub+zip, application/octet-stream")
    .call()?;
  let status = response.status().as_u16();
  if !(200..=299).contains(&status) {
    return Err(Error::Status(status));
  }
  let tmp = path.with_extension(format!("epub.{}.tmp", std::process::id()));
  let result = (|| {
    let mut file = OpenOptions::new()
      .write(true)
      .create_new(true)
      .open(&tmp)
      .map_err(|e| Error::Io(tmp.clone(), e))?;
    std::io::copy(&mut response.body_mut().as_reader(), &mut file)
      .map_err(|e| Error::Io(tmp.clone(), e))?;
    file.flush().map_err(|e| Error::Io(tmp.clone(), e))?;
    file.sync_all().map_err(|e| Error::Io(tmp.clone(), e))?;
    fs::rename(&tmp, path).map_err(|e| Error::Io(path.to_path_buf(), e))
  })();
  if result.is_err() {
    let _ = fs::remove_file(&tmp);
  }
  result
}

/// Reads the endpoint Nickel already uses for native Kobo sync. The token is
/// deliberately returned only to the HTTP client and never printed.
pub fn api_endpoint(root: &Path) -> Option<String> {
  let text = fs::read_to_string(root.join(CONF)).ok()?;
  let mut section = "";
  for raw in text.lines() {
    let line = raw.trim();
    if line.starts_with('[') && line.ends_with(']') {
      section = &line[1..line.len() - 1];
      continue;
    }
    if section != "OneStoreServices" {
      continue;
    }
    let Some((key, value)) = line.split_once('=') else {
      continue;
    };
    if key.trim() != "api_endpoint" {
      continue;
    }
    let value = value.trim().trim_matches('"').trim_end_matches('/');
    if value.contains("/kobo/") && !value.chars().any(char::is_whitespace) {
      return Some(value.to_string());
    }
  }
  None
}

fn original_url(url: &str) -> Result<String, Error> {
  let Some((base, format)) = url.rsplit_once('/') else {
    return Err(Error::Response("download URL has no format".into()));
  };
  if !format.eq_ignore_ascii_case("epub") && !format.eq_ignore_ascii_case("kepub") {
    return Err(Error::Response("download URL has an unknown format".into()));
  }
  Ok(format!("{base}/epub"))
}

fn cache_name(content_id: &str) -> String {
  use md5::{Digest, Md5};
  Md5::digest(content_id.as_bytes())
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

fn is_uuid(value: &str) -> bool {
  if value.len() != 36 {
    return false;
  }
  value.bytes().enumerate().all(|(i, byte)| {
    if matches!(i, 8 | 13 | 18 | 23) {
      byte == b'-'
    } else {
      byte.is_ascii_hexdigit()
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reads_cwa_endpoint_from_kobo_conf() {
    let dir = tempfile_dir(line!());
    let path = dir.join(CONF);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
      &path,
      "[OneStoreServices]\napi_endpoint = https://books.test/kobo/secret/\n",
    )
    .unwrap();
    assert_eq!(
      api_endpoint(&dir).as_deref(),
      Some("https://books.test/kobo/secret")
    );
    fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn ignores_an_endpoint_in_another_section() {
    let dir = tempfile_dir(line!());
    let path = dir.join(CONF);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
      &path,
      "[Other]\napi_endpoint=https://books.test/kobo/secret\n",
    )
    .unwrap();
    assert_eq!(api_endpoint(&dir), None);
    fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn changes_kepub_download_to_original_epub() {
    assert_eq!(
      original_url("https://books.test/kobo/token/download/146/kepub").unwrap(),
      "https://books.test/kobo/token/download/146/epub"
    );
  }

  #[test]
  fn only_uuid_content_ids_are_cwa_candidates() {
    assert!(is_uuid("8e273f4a-688c-468b-b2c9-425bd0ee1c4b"));
    assert!(!is_uuid("file:///mnt/onboard/Books/book.kepub.epub"));
    assert!(!is_uuid("8e273f4a-688c-468b-b2c9-425bd0ee1c4"));
  }

  fn tempfile_dir(suffix: u32) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
      "nickelinksync-cwa-test-{}-{suffix}",
      std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    path
  }
}
