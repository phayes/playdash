//! The blocking API's segment transport: the source of segment bodies and sizes.

use super::{FetchedManifest, SegmentLocation, slice_range};
use crate::error::Error;
use bytes::Bytes;
use std::io::Read;
use std::time::Duration;
use ureq::ResponseExt;

/// Fetches segment bodies and sizes for a [`SegmentCache`](super::SegmentCache).
///
/// [`ureq::Agent`] implements this with its connection pool.
/// Implement this trait to load segments from another HTTP client, local
/// storage, or an application-specific source.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use bytes::Bytes;
/// use playdash::Error;
/// use playdash::segment::{SegmentCache, SegmentLocation, Transport};
///
/// struct MemoryTransport;
///
/// impl Transport for MemoryTransport {
///     fn get(&self, segment: &SegmentLocation, _: Option<u64>) -> Result<Bytes, Error> {
///         let bytes: &'static [u8] = match segment.url.as_str() {
///             "memory://init" => b"initialization data",
///             _ => b"media data",
///         };
///         Ok(Bytes::from_static(bytes))
///     }
/// }
///
/// let cache = SegmentCache::new(Arc::new(MemoryTransport));
/// assert!(cache.cached(&SegmentLocation::new("memory://init")).is_none());
/// ```
pub trait Transport: Send + Sync {
    /// Body of `segment`.
    ///
    /// Must honor [`SegmentLocation::range`] and return exactly those bytes; a ranged
    /// body of any other length fails the stream. `size_hint` is the length
    /// when already known, for preallocating the body.
    fn get(&self, segment: &SegmentLocation, size_hint: Option<u64>) -> Result<Bytes, Error>;

    /// Byte length of `segment`, typically from a HEAD `Content-Length`.
    ///
    /// Never called for ranged segments, whose sizes are already known. An
    /// error disables HEAD probing for the stream; the default always errors.
    fn head(&self, segment: &SegmentLocation) -> Result<u64, Error> {
        Err(Error::Initialization(format!(
            "transport cannot HEAD {segment}"
        )))
    }

    /// Fetches the manifest at `url` and returns its text and final URL.
    ///
    /// The default uses [`Self::get`] and cannot discover redirects, so it
    /// reports `url` unchanged. It fails on transport errors or invalid UTF-8.
    fn fetch_manifest(&self, url: &str) -> Result<FetchedManifest, Error> {
        let body = self.get(&SegmentLocation::new(url), None)?;
        FetchedManifest::from_bytes(body, url)
    }
}

impl Transport for ureq::Agent {
    /// Sends a `Range` header for ranged segments; if a server ignores it and
    /// returns the whole body, the range is sliced out of that body.
    fn get(&self, segment: &SegmentLocation, size_hint: Option<u64>) -> Result<Bytes, Error> {
        let mut request = ureq::Agent::get(self, &segment.url);
        if let Some(range) = &segment.range {
            request = request.header(
                ureq::http::header::RANGE,
                format!("bytes={}-{}", range.start(), range.end()),
            );
        }
        let response = request.call()?;
        let partial = response.status() == ureq::http::StatusCode::PARTIAL_CONTENT;
        let mut bytes = size_hint
            .and_then(|len| usize::try_from(len).ok())
            .map(Vec::with_capacity)
            .unwrap_or_default();
        response.into_body().into_reader().read_to_end(&mut bytes)?;
        let body = Bytes::from(bytes);
        match &segment.range {
            Some(range) if !partial => {
                #[cfg(feature = "log")]
                log::warn!("server ignored the Range request for {segment}; slicing the full body");
                Ok(slice_range(&body, range)
                    .map_or_else(|| body.clone(), |part| body.slice_ref(part)))
            }
            _ => Ok(body),
        }
    }

    fn head(&self, segment: &SegmentLocation) -> Result<u64, Error> {
        let url = &segment.url;
        let response = ureq::Agent::head(self, url).call()?;
        response
            .headers()
            .get(ureq::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                Error::Initialization(format!("HEAD for {url} did not return Content-Length"))
            })
    }

    /// Fetches a manifest, following redirects and preserving the final URL.
    fn fetch_manifest(&self, url: &str) -> Result<FetchedManifest, Error> {
        let response = ureq::Agent::get(self, url)
            .call()
            .map_err(Error::DashManifestFetch)?;
        let url = response.get_uri().to_string();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(Error::DashManifestFetch)?;
        Ok(FetchedManifest { body, url })
    }
}

/// Maximum idle time for an HTTP connection in the ureq pool.
///
/// Fastly allows idle client reuse for up to 10 minutes. Stay well under
/// that so we close first, and under typical NAT idle timeouts.
const POOL_IDLE_AGE: Duration = Duration::from_secs(90);

/// A ureq agent with this crate's connection-pool settings.
pub(crate) fn ureq_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .max_idle_age(POOL_IDLE_AGE)
            .build(),
    )
}
