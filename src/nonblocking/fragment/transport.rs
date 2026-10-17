//! The async API's fragment transport: the source of fragment bodies and sizes.

use crate::error::Error;
use crate::fragment::{Fragment, slice_range};
use bytes::Bytes;
use futures_util::future::BoxFuture;
use reqwest::header::{CONTENT_LENGTH, RANGE};
use reqwest::{Client, Response, StatusCode};

/// Fetches fragment bodies and sizes for a [`FragmentCache`](super::FragmentCache):
/// the async counterpart of the blocking `playdash::fragment::Transport`.
///
/// [`reqwest::Client`] implements this. Implement it to load fragments from
/// another HTTP client, local storage, or an application-specific source.
/// Each method returns a boxed future, so a transport can be shared as
/// `Arc<dyn Transport>`.
///
/// # Examples
///
/// ```
/// use futures_util::future::BoxFuture;
/// use playdash::fragment::Fragment;
/// use playdash::nonblocking::fragment::Transport;
/// use playdash::{Bytes, Error};
///
/// struct MemoryTransport;
///
/// impl Transport for MemoryTransport {
///     fn get<'a>(
///         &'a self,
///         fragment: &'a Fragment,
///         _size_hint: Option<u64>,
///     ) -> BoxFuture<'a, Result<Bytes, Error>> {
///         Box::pin(async move {
///             Ok(Bytes::from_static(match fragment.url.as_str() {
///                 "memory://init" => b"initialization data",
///                 _ => b"media data",
///             }))
///         })
///     }
/// }
/// ```
pub trait Transport: Send + Sync {
    /// Body of `fragment`.
    ///
    /// Must honor [`Fragment::range`] and return exactly those bytes; a ranged
    /// body of any other length fails the stream. `size_hint` is the length
    /// when already known, for preallocating the body.
    fn get<'a>(
        &'a self,
        fragment: &'a Fragment,
        size_hint: Option<u64>,
    ) -> BoxFuture<'a, Result<Bytes, Error>>;

    /// Byte length of `fragment`, typically from a HEAD `Content-Length`.
    ///
    /// Never called for ranged fragments, whose sizes are already known. An
    /// error disables HEAD probing for the stream; the default always errors.
    fn head<'a>(&'a self, fragment: &'a Fragment) -> BoxFuture<'a, Result<u64, Error>> {
        Box::pin(async move {
            Err(Error::Initialization(format!(
                "transport cannot HEAD {fragment}"
            )))
        })
    }
}

impl Transport for Client {
    /// Sends a `Range` header for ranged fragments; if a server ignores it and
    /// returns the whole body, the range is sliced out of that body.
    fn get<'a>(
        &'a self,
        fragment: &'a Fragment,
        _size_hint: Option<u64>,
    ) -> BoxFuture<'a, Result<Bytes, Error>> {
        Box::pin(async move {
            let failed = |source| Error::FragmentRequest {
                fragment: fragment.to_string(),
                source,
            };
            let mut request = Client::get(self, &fragment.url);
            if let Some(range) = &fragment.range {
                request = request.header(RANGE, format!("bytes={}-{}", range.start(), range.end()));
            }
            let response = request
                .send()
                .await
                .and_then(Response::error_for_status)
                .map_err(failed)?;
            let partial = response.status() == StatusCode::PARTIAL_CONTENT;
            let body = response.bytes().await.map_err(failed)?;
            Ok(match &fragment.range {
                Some(range) if !partial => {
                    #[cfg(feature = "log")]
                    log::warn!(
                        "server ignored the Range request for {fragment}; slicing the full body"
                    );
                    slice_range(&body, range)
                        .map_or_else(|| body.clone(), |part| body.slice_ref(part))
                }
                _ => body,
            })
        })
    }

    fn head<'a>(&'a self, fragment: &'a Fragment) -> BoxFuture<'a, Result<u64, Error>> {
        Box::pin(async move {
            let url = &fragment.url;
            let response = Client::head(self, url)
                .send()
                .await
                .and_then(Response::error_for_status)
                .map_err(|source| Error::FragmentRequest {
                    fragment: fragment.to_string(),
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
}
