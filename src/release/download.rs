//! Reading the public GitHub release over HTTPS with no token.

use std::time::Duration;

use ureq::Agent;

/// A release archive larger than this is refused rather than read.
const ARCHIVE_LIMIT: u64 = 256 * 1024 * 1024;
/// A `.sha256` sidecar is one short line.
const SIDECAR_LIMIT: u64 = 4 * 1024;
const MAX_REDIRECTS: u32 = 5;

/// Where releases come from. Only tests build one other than [`Source::github`]: a loopback
/// HTTP server, which is why `https_only` is a field and not a constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Source {
    pub(super) site: String,
    pub(super) repository: String,
    pub(super) https_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Latest {
    Tag(String),
    /// The repository has no published (non-prerelease) release yet.
    None,
}

impl Source {
    pub(super) fn github() -> Self {
        Self {
            site: "https://github.com".to_owned(),
            repository: super::REPOSITORY.to_owned(),
            https_only: true,
        }
    }

    pub(super) fn asset_url(&self, tag: &str, asset: &str) -> String {
        format!(
            "{}/{}/releases/download/{tag}/{asset}",
            self.site, self.repository
        )
    }

    fn agent(&self, redirects: u32) -> Agent {
        // A loopback test server is reached directly, never through an ambient proxy.
        let proxy = if self.https_only {
            ureq::Proxy::try_from_env()
        } else {
            None
        };
        Agent::config_builder()
            .https_only(self.https_only)
            .proxy(proxy)
            .max_redirects(redirects)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_global(Some(Duration::from_mins(10)))
            .user_agent(format!("loadout/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    }

    /// The tag `releases/latest` redirects to. GitHub answers with a redirect to
    /// `…/releases/tag/<tag>`, which is read without following it, so no API, token, or JSON is
    /// involved. A 404 means nothing is published yet.
    pub(super) fn latest(&self) -> Result<Latest, String> {
        let url = format!("{}/{}/releases/latest", self.site, self.repository);
        let response = match self.agent(0).get(&url).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(Latest::None),
            Err(error) => return Err(format!("{url}: {error}")),
        };
        if !response.status().is_redirection() {
            return Err(format!(
                "{url} answered {} instead of a redirect",
                response.status()
            ));
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| format!("{url} redirected without a readable location"))?;
        let prefix = format!("/{}/releases/tag/", self.repository);
        let tag = location
            .split_once(&prefix)
            .map(|(_, tag)| tag)
            .filter(|tag| {
                !tag.is_empty()
                    && tag
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
            })
            .ok_or_else(|| format!("{url} redirected to {location:?}, not a release tag"))?;
        Ok(Latest::Tag(tag.to_owned()))
    }

    pub(super) fn archive(&self, url: &str) -> Result<Vec<u8>, String> {
        self.get(url, ARCHIVE_LIMIT)
    }

    pub(super) fn sidecar(&self, url: &str) -> Result<Vec<u8>, String> {
        self.get(url, SIDECAR_LIMIT)
    }

    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, String> {
        let mut response = self
            .agent(MAX_REDIRECTS)
            .get(url)
            .call()
            .map_err(|error| format!("{url}: {error}"))?;
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|error| format!("{url}: {error}"))
    }
}

/// The digest a `.sha256` sidecar names, when the sidecar is exactly the ASCII bytes
/// `<64 lowercase hex>  <asset>\n` and nothing else: no byte order mark, CR, other encoding,
/// extra line, or other name.
pub(super) fn sidecar_digest(bytes: &[u8], asset: &str) -> Result<String, String> {
    let refuse = || format!("{asset}.sha256 is not exactly one '<sha256>  {asset}' line");
    let text = std::str::from_utf8(bytes).map_err(|_| refuse())?;
    let line = text.strip_suffix('\n').ok_or_else(refuse)?;
    let (digest, name) = line.split_once("  ").ok_or_else(refuse)?;
    let lower_hex = digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !lower_hex || name != asset || !text.is_ascii() {
        return Err(refuse());
    }
    Ok(digest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_must_be_the_exact_line() {
        let asset = "loadout-x86_64-unknown-linux-musl.tar.gz";
        let digest = "d".repeat(64);
        let line = format!("{digest}  {asset}\n");
        assert_eq!(sidecar_digest(line.as_bytes(), asset).unwrap(), digest);
        let utf16: Vec<u8> = line.encode_utf16().flat_map(u16::to_le_bytes).collect();
        for bad in [
            format!("{digest}  {asset}").into_bytes(),
            format!("{digest}  {asset}\n\n").into_bytes(),
            format!("{digest}  {asset}\r\n").into_bytes(),
            format!("{digest}  {asset}\nextra\n").into_bytes(),
            format!("{}  {asset}\n", digest.to_uppercase()).into_bytes(),
            format!("{digest}  loadout-other.tar.gz\n").into_bytes(),
            format!("{digest} {asset}\n").into_bytes(),
            format!("{digest}\u{a0} {asset}\n").into_bytes(),
            [b"\xef\xbb\xbf".as_slice(), line.as_bytes()].concat(),
            [line.trim_end().as_bytes(), b"\0"].concat(),
            utf16,
        ] {
            assert!(sidecar_digest(&bad, asset).is_err(), "{bad:?}");
        }
    }
}
