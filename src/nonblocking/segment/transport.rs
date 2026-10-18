//! The async API's segment transport: the source of segment bodies and sizes.

use crate::error::Error;
use crate::segment::{FetchedManifest, SegmentLocation, slice_range};
use bytes::Bytes;
use futures_util::future::BoxFuture;
use reqwest::header::{CONTENT_LENGTH, RANGE};
use reqwest::{Client, Response, StatusCode};

/// Fetches segment bodies and sizes for a [`SegmentCache`](super::SegmentCache):
/// the async counterpart of the blocking [`Transport`](crate::segment::Transport).
///
/// [`reqwest::Client`] implements this. Implement it to load segments from
/// another HTTP client, local storage, or an application-specific source.
/// Each method returns a boxed future, so a transport can be shared as
/// `Arc<dyn Transport>`.
///
/// # Examples
///
/// ```
/// use futures_util::future::BoxFuture;
/// use playdash::segment::SegmentLocation;
/// use playdash::nonblocking::segment::Transport;
/// use playdash::{Bytes, Error};
///
/// struct MemoryTransport;
///
/// impl Transport for MemoryTransport {
///     fn get<'a>(
///         &'a self,
///         segment: &'a SegmentLocation,
///         _size_hint: Option<u64>,
///     ) -> BoxFuture<'a, Result<Bytes, Error>> {
///         Box::pin(async move {
///             Ok(Bytes::from_static(match segment.url.as_str() {
///                 "memory://init" => b"initialization data",
///                 _ => b"media data",
///             }))
///         })
///     }
/// }
/// ```
pub trait Transport: Send + Sync {
    /// Body of `segment`.
    ///
    /// Must honor [`SegmentLocation::range`] and return exactly those bytes; a ranged
    /// body of any other length fails the stream. `size_hint` is the length
    /// when already known, for preallocating the body.
    fn get<'a>(
        &'a self,
        segment: &'a SegmentLocation,
        size_hint: Option<u64>,
    ) -> BoxFuture<'a, Result<Bytes, Error>>;

    /// Byte length of `segment`, typically from a HEAD `Content-Length`.
    ///
    /// Never called for ranged segments, whose sizes are already known. An
    /// error disables HEAD probing for the stream; the default always errors.
    fn head<'a>(&'a self, segment: &'a SegmentLocation) -> BoxFuture<'a, Result<u64, Error>> {
        Box::pin(async move {
            Err(Error::Initialization(format!(
                "transport cannot HEAD {segment}"
            )))
        })
    }

    /// Fetches the manifest at `url` and returns its text and final URL.
    ///
    /// The default uses [`Self::get`] and cannot discover redirects, so it
    /// reports `url` unchanged. It fails on transport errors or invalid UTF-8.
    fn fetch_manifest<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<FetchedManifest, Error>> {
        Box::pin(async move {
            let body = self.get(&SegmentLocation::new(url), None).await?;
            FetchedManifest::from_bytes(body, url)
        })
    }
}

impl Transport for Client {
    /// Sends a `Range` header for ranged segments; if a server ignores it and
    /// returns the whole body, the range is sliced out of that body.
    fn get<'a>(
        &'a self,
        segment: &'a SegmentLocation,
        _size_hint: Option<u64>,
    ) -> BoxFuture<'a, Result<Bytes, Error>> {
        Box::pin(async move {
            let failed = |source| Error::SegmentRequest {
                segment: segment.to_string(),
                source,
            };
            let mut request = Client::get(self, &segment.url);
            if let Some(range) = &segment.range {
                request = request.header(RANGE, format!("bytes={}-{}", range.start(), range.end()));
            }
            let response = request
                .send()
                .await
                .and_then(Response::error_for_status)
                .map_err(failed)?;
            let partial = response.status() == StatusCode::PARTIAL_CONTENT;
            let body = response.bytes().await.map_err(failed)?;
            Ok(match &segment.range {
                Some(range) if !partial => {
                    #[cfg(feature = "log")]
                    log::warn!(
                        "server ignored the Range request for {segment}; slicing the full body"
                    );
                    slice_range(&body, range)
                        .map_or_else(|| body.clone(), |part| body.slice_ref(part))
                }
                _ => body,
            })
        })
    }

    fn head<'a>(&'a self, segment: &'a SegmentLocation) -> BoxFuture<'a, Result<u64, Error>> {
        Box::pin(async move {
            let url = &segment.url;
            let response = Client::head(self, url)
                .send()
                .await
                .and_then(Response::error_for_status)
                .map_err(|source| Error::SegmentRequest {
                    segment: segment.to_string(),
                    source,
                })?;
            response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    Error::Initialization(format!("HEAD for {url} did not return Content-Length"))
                })
        })
    }

    /// Fetches a manifest, following redirects and preserving the final URL.
    fn fetch_manifest<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<FetchedManifest, Error>> {
        Box::pin(async move {
            let response = Client::get(self, url)
                .send()
                .await
                .and_then(Response::error_for_status)
                .map_err(Error::DashManifestRequest)?;
            let url = response.url().to_string();
            let body = response.text().await.map_err(Error::DashManifestRequest)?;
            Ok(FetchedManifest { body, url })
        })
    }
}
