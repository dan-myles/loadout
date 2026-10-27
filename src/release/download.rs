//! Reading the public GitHub release over HTTPS with no token.

use std::time::Duration;

use semver::Version;
use ureq::http::Response;
use ureq::{Agent, Body};

/// A release archive larger than this is refused rather than read.
const ARCHIVE_LIMIT: u64 = 256 * 1024 * 1024;
/// A `.sha256` sidecar is one short line.
const SIDECAR_LIMIT: u64 = 4 * 1024;
const MAX_REDIRECTS: u32 = 5;
/// One page of the releases list larger than this is refused rather than read.
const LIST_LIMIT: u64 = 8 * 1024 * 1024;
/// The releases list is read 100 releases to a page, and no further than this many pages.
const LIST_PAGES: usize = 10;
const PER_PAGE: u32 = 100;

/// Where releases come from. Only tests build one other than [`Source::github`]: a loopback
/// HTTP server, which is why `https_only` is a field and not a constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Source {
    pub(super) site: String,
    /// The REST API's base, for the releases list that `--prerelease` reads.
    pub(super) api: String,
    pub(super) repository: String,
    pub(super) https_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Latest {
    Tag(String),
    /// The repository has no published release of the kind asked for yet.
    None,
}

/// What the releases list says about one release; everything else in it is ignored.
#[derive(serde::Deserialize)]
struct Listed {
    tag_name: String,
    #[serde(default)]
    draft: bool,
}

impl Source {
    pub(super) fn github() -> Self {
        Self {
            site: "https://github.com".to_owned(),
            api: "https://api.github.com".to_owned(),
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

    /// An agent for this source. With `status_errors`, any status from 400 up is an error;
    /// without it, the caller reads the status itself.
    fn agent(&self, redirects: u32, status_errors: bool) -> Agent {
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
            .http_status_as_error(status_errors)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_global(Some(Duration::from_mins(10)))
            .user_agent(format!("loadout/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    }

    /// The tag `releases/latest` redirects to. GitHub answers with a redirect to
    /// `…/releases/tag/<tag>`, which is read without following it, so no API, token, or JSON is
    /// involved. With no stable release published (none at all, or only prereleases), github.com
    /// redirects to the repository's release list, `<site>/<repository>/releases`, instead; that,
    /// or a 404, means there is no release. Any other location is an error.
    pub(super) fn latest(&self) -> Result<Latest, String> {
        let url = format!("{}/{}/releases/latest", self.site, self.repository);
        let response = match self.agent(0, true).get(&url).call() {
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
        let list = format!("/{}/releases", self.repository);
        if location == list || location == format!("{}{list}", self.site) {
            return Ok(Latest::None);
        }
        let prefix = format!("{list}/tag/");
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

    /// The newest published release of any kind, prereleases included: the `v<semver>` tag of
    /// highest semver precedence in the public releases list (`/repos/<repository>/releases`, no
    /// token). Build metadata has no precedence, so of tags that differ only in it the first
    /// listed (GitHub lists the newest first) is kept. Drafts and tags that aren't `v<semver>`
    /// are skipped. Every page the list links to is read, up to [`LIST_PAGES`] pages of at most
    /// [`LIST_LIMIT`] bytes each, and only pages under this source's API base are followed. A
    /// longer list, an answer that isn't a release list, or a request GitHub refuses (its API
    /// allows 60 unauthenticated requests an hour) is an error, never an empty list.
    pub(super) fn newest(&self) -> Result<Latest, String> {
        let agent = self.agent(MAX_REDIRECTS, false);
        let mut url = format!(
            "{}/repos/{}/releases?per_page={PER_PAGE}",
            self.api, self.repository
        );
        let mut newest: Option<(Version, String)> = None;
        for _ in 0..LIST_PAGES {
            let mut response = agent
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .call()
                .map_err(|error| format!("{url}: {error}"))?;
            let status = response.status();
            if status == 403 || status == 429 {
                return Err(
                    match (
                        header(&response, "x-ratelimit-remaining").as_deref(),
                        header(&response, "x-ratelimit-reset"),
                    ) {
                        (Some("0"), Some(reset)) => format!(
                            "{url}: GitHub's limit on unauthenticated API requests is used up until {reset} (Unix time); try again after that"
                        ),
                        _ => format!("{url}: GitHub refused the request ({status})"),
                    },
                );
            }
            if !status.is_success() {
                return Err(format!("{url} answered {status}"));
            }
            let next = next_page(&response, &self.api)?;
            let body = response
                .body_mut()
                .with_config()
                .limit(LIST_LIMIT)
                .read_to_vec()
                .map_err(|error| format!("{url}: {error}"))?;
            let listed: Vec<Listed> = serde_json::from_slice(&body)
                .map_err(|error| format!("{url} didn't answer with a release list: {error}"))?;
            for release in listed.into_iter().filter(|release| !release.draft) {
                if let Some(version) = tag_version(&release.tag_name)
                    && newest
                        .as_ref()
                        .is_none_or(|(best, _)| version.cmp_precedence(best).is_gt())
                {
                    newest = Some((version, release.tag_name));
                }
            }
            match next {
                Some(link) => url = link,
                None => return Ok(newest.map_or(Latest::None, |(_, tag)| Latest::Tag(tag))),
            }
        }
        Err(format!(
            "the release list of {} is longer than {LIST_PAGES} pages; loadout reads no further",
            self.repository
        ))
    }

    pub(super) fn archive(&self, url: &str) -> Result<Vec<u8>, String> {
        self.get(url, ARCHIVE_LIMIT)
    }

    pub(super) fn sidecar(&self, url: &str) -> Result<Vec<u8>, String> {
        self.get(url, SIDECAR_LIMIT)
    }

    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, String> {
        let mut response = self
            .agent(MAX_REDIRECTS, true)
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

/// The version a release tag names: `v` and a semantic version, in the characters a tag may hold.
pub(super) fn tag_version(tag: &str) -> Option<Version> {
    let plain = tag
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b));
    tag.strip_prefix('v')
        .filter(|_| plain)
        .and_then(|version| Version::parse(version).ok())
}

fn header(response: &Response<Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The next page of a paged GitHub answer: the `rel="next"` target of its `Link` header, which
/// must be under `api`. `None` on the last page.
fn next_page(response: &Response<Body>, api: &str) -> Result<Option<String>, String> {
    let Some(link) = response.headers().get("link") else {
        return Ok(None);
    };
    let link = link
        .to_str()
        .map_err(|_| "the release list's Link header isn't text".to_owned())?;
    for part in link.split(',') {
        let (target, params) = part.trim().split_once(';').unwrap_or((part.trim(), ""));
        if !params
            .split(';')
            .any(|param| param.trim() == "rel=\"next\"")
        {
            continue;
        }
        let url = target
            .trim()
            .strip_prefix('<')
            .and_then(|target| target.strip_suffix('>'))
            .ok_or_else(|| format!("the release list's Link header is malformed: {link}"))?;
        if !url.starts_with(&format!("{api}/")) {
            return Err(format!(
                "the release list links its next page outside {api}: {url}"
            ));
        }
        return Ok(Some(url.to_owned()));
    }
    Ok(None)
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
