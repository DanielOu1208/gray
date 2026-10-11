//! Download (https-only, size-capped, hash-verified) + tar.gz unpack.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;

/// Max download size: 64 MiB.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// HTTP client for plugin downloads: redirects allowed only when every hop
/// passes [`check_url`] (no https→http downgrade), with connect/total
/// deadlines so slow bodies expire.
pub fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if check_url(attempt.url().as_str()).is_ok() {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()
}

/// Strip userinfo, query, and fragment so URLs are safe to log.
/// Renders `https://u:p@h/x?token=1` as `https://h/x`.
pub fn redact(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, after) = rest.split_at(auth_end);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let path = after
        .split('#')
        .next()
        .unwrap_or(after)
        .split('?')
        .next()
        .unwrap_or(after);
    match scheme {
        Some(s) => format!("{s}://{host}{path}"),
        None => format!("{host}{path}"),
    }
}

/// https-only, except http loopback (127.0.0.1/::1/localhost) for tests.
pub(crate) fn check_url(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| anyhow::anyhow!("invalid plugin URL: {}", redact(url)))?;
    if parsed.scheme() == "https"
        || (parsed.scheme() == "http"
            && matches!(parsed.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")))
    {
        return Ok(());
    }
    anyhow::bail!("refusing non-https plugin URL: {}", redact(url));
}

/// Decode standard base64 (npm `integrity` payloads) via the workspace
/// `base64` crate. Empty input is rejected explicitly: the crate decodes
/// `""` to empty, but an empty digest must never verify.
fn b64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;
    if s.is_empty() {
        anyhow::bail!("invalid base64 hash");
    }
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|_| anyhow::anyhow!("invalid base64 hash"))
}

/// Stream `url` to `$GRAY_HOME/plugins/tmp/`, enforcing the 64 MiB cap and
/// verifying against `expected` (`"sha256:<hex>"` or npm's `"sha512-<base64>"`).
/// Anything else bails `unsupported hash algorithm`. The temp file is
/// auto-deleted on failure; on success the kept path is returned.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    expected: Option<&crate::index::HashSpec>,
) -> anyhow::Result<PathBuf> {
    check_url(url)?;
    // An index entry that fails to pin a digest must not download: only an
    // explicit direct-URL install (`expected == None`, warned at the call
    // site) may skip verification.
    if let Some(spec) = expected
        && spec.primary().is_none_or(|s| s.is_empty())
    {
        anyhow::bail!("plugin download requires an expected digest");
    }
    let tmp_dir = crate::plugins_dir().join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    log::debug!("downloading plugin archive from {}", redact(url));
    let mut resp = client.get(url).send().await?.error_for_status()?;
    // The redirect policy `stop()`s on a blocked hop and hands the 3xx back:
    // `error_for_status` does not treat 3xx as an error, so reject it here
    // instead of saving the redirect body as the archive.
    if resp.status().is_redirection() {
        anyhow::bail!("download redirected to a disallowed URL");
    }

    let tmp = tempfile::NamedTempFile::new_in(&tmp_dir)?;
    let temppath = tmp.into_temp_path();
    let path: PathBuf = temppath.to_path_buf();
    let mut file = tokio::fs::File::create(&path).await?;
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    let mut total: u64 = 0;
    loop {
        let chunk = resp.chunk().await?;
        let Some(bytes) = chunk else { break };
        total += bytes.len() as u64;
        if total > MAX_BYTES {
            drop(file);
            // `temppath` drops here and deletes the partial file.
            anyhow::bail!("plugin archive exceeds 64 MiB cap");
        }
        sha256.update(&bytes);
        sha512.update(&bytes);
        file.write_all(&bytes).await?;
    }
    file.flush().await?;
    drop(file);

    if let Some(spec) = expected
        && let Some(want) = spec.primary()
    {
        let (algo, hex) = match want.split_once(':') {
            Some((a, h)) => (a, h),
            None => ("", want),
        };
        if algo.eq_ignore_ascii_case("sha256") {
            let got = format!("{:x}", sha256.finalize());
            if !got.eq_ignore_ascii_case(hex) {
                anyhow::bail!("hash mismatch for {}", redact(url));
            }
        } else if let Some(payload) = want.strip_prefix("sha512-") {
            let want_raw = b64_decode(payload)
                .map_err(|_| anyhow::anyhow!("invalid sha512 integrity for {}", redact(url)))?;
            if sha512.finalize()[..] != want_raw[..] {
                anyhow::bail!("hash mismatch for {}", redact(url));
            }
        } else {
            anyhow::bail!("unsupported hash algorithm in index entry");
        }
    }

    temppath
        .keep()
        .map_err(|e| anyhow::anyhow!("keeping download: {e}"))?;
    Ok(path)
}

/// True when an archive entry name cannot be safely joined onto the
/// destination. Beyond the obvious absolute and `..` cases this refuses
/// Windows drive names: `C:\evil` and `C:evil` are neither absolute nor
/// parent components, but `dest.join()` with a drive-prefixed path
/// *replaces* the base on Windows, so the entry lands outside the install
/// directory. A gray-native archive never legitimately contains a drive
/// name, so the refusal is safe everywhere.
fn unsafe_entry_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let drive_name = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    drive_name
        || name.starts_with('/')
        || name.starts_with('\\')
        || Path::new(name).components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
}

/// Belt-and-braces containment: the joined target must still live inside
/// the destination after the name check above.
fn confined(dest: &Path, rel: &Path) -> bool {
    dest.join(rel).starts_with(dest)
}

/// Unpack a tar.gz, rejecting absolute paths and `..` entries.
/// Total expanded output is capped at 256 MiB and entry counts/types are
/// bounded (regular files and directories only).
pub fn unpack_tar_gz(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    use std::io::Read as _;
    const MAX_OUT: u64 = 256 * 1024 * 1024;
    const MAX_ENTRIES: usize = 10_000;
    let file = std::fs::File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(file).take(MAX_OUT + 1);
    let mut ar = tar::Archive::new(gz);
    let mut total = 0u64;
    for (index, entry) in ar.entries()?.enumerate() {
        anyhow::ensure!(index < MAX_ENTRIES, "too many archive entries");
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        anyhow::ensure!(kind.is_file() || kind.is_dir(), "unsupported archive entry");
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| anyhow::anyhow!("archive size overflow"))?;
        anyhow::ensure!(total <= MAX_OUT, "tar archive exceeds output cap");
        let path = entry.path()?.into_owned();
        if unsafe_entry_name(&path.to_string_lossy()) || !confined(dest, &path) {
            anyhow::bail!("refusing unsafe archive entry: {}", path.display());
        }
        entry.unpack_in(dest)?;
    }
    // Compressed-input ceiling (download caps apply earlier; this is
    // belt-and-braces). Real output is bounded by the header-size
    // accounting above: tar only ever emits header.size() bytes per entry.
    anyhow::ensure!(
        ar.into_inner().limit() != 0,
        "tar archive exceeds input cap"
    );
    Ok(())
}

#[path = "fetch_tests.rs"]
#[cfg(test)]
mod tests;
