//! The async API of the `async` feature: manifests and fragments fetched
//! through [`reqwest`] on a tokio runtime.
//!
//! This module mirrors the blocking API of the `sync` feature, with the same
//! names: [`DashManifest`], [`FragmentStream`], [`FragmentCache`] and
//! [`Transport`]. Here [`DashManifest::new_from_url`] and
//! [`DashManifest::stream`] are awaited, and a [`FragmentStream`] is a
//! [`futures_util::Stream`] of whole fragments and a tokio
//! [`AsyncRead`](tokio::io::AsyncRead) and [`AsyncSeek`](tokio::io::AsyncSeek)
//! view of the same bytes.
//!
//! # Examples
//!
//! ```no_run
//! use futures_util::StreamExt;
//! use playdash::nonblocking::DashManifest;
//!
//! # async fn example() -> Result<(), playdash::Error> {
//! let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
//! let mut fragments = manifest.stream("audio").await?;
//! while let Some(fragment) = fragments.next().await {
//!     let bytes = fragment?;
//!     // Append `bytes` to your buffer.
//!     # drop(bytes);
//! }
//! # Ok(())
//! # }
//! ```

mod cache;
mod packet_stream;
mod stream;
mod transport;

pub use crate::decoder::nonblocking::DecoderPacketStream;
pub use cache::FragmentCache;
pub use packet_stream::PacketStream;
pub use stream::FragmentStream;
pub use transport::Transport;

use crate::error::Error;
use crate::fragment::{Fragments, MediaTimeline};
use crate::manifest::Representation;
use reqwest::{Client, Response};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// A parsed MPEG-DASH manifest whose representations stream asynchronously:
/// the async counterpart of the blocking
/// [`playdash::DashManifest`](crate::DashManifest), with the same constructors
/// and queries.
///
/// It fetches through a [`reqwest::Client`] of its own, with rustls, unless
/// given one with [`Self::with_client`]. Parsing never touches the network.
///
/// # Examples
///
/// ```no_run
/// use playdash::nonblocking::DashManifest;
///
/// # async fn example(client: reqwest::Client, xml: String) -> Result<(), playdash::Error> {
/// // A manifest that arrived some other way, fetched through the caller's client.
/// let manifest = DashManifest::new(xml)?
///     .with_client(client)
///     .with_base_url("https://media.example/stream.mpd")?;
/// let stream = manifest.stream("FLAC").await?;
/// # drop(stream);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct DashManifest {
    manifest: crate::DashManifest,
    /// Made on first use unless the caller gave one, so a caller that brings
    /// its own never pays for building a default.
    client: OnceLock<Client>,
}

impl DashManifest {
    /// Parses MPEG-DASH XML. See [`crate::DashManifest::new`].
    pub fn new(dash_xml: impl AsRef<str>) -> Result<Self, Error> {
        crate::DashManifest::new(dash_xml).map(Self::from)
    }

    /// Decodes a base64 `data:` URL and parses the embedded MPEG-DASH XML.
    pub fn new_from_data_url(data_url: &str) -> Result<Self, Error> {
        crate::DashManifest::new_from_data_url(data_url).map(Self::from)
    }

    /// Fetches and parses an MPEG-DASH manifest, and loads any `SegmentBase`
    /// indexes it needs.
    ///
    /// The final URL, after redirects, becomes the [`Self::base_url`] that
    /// relative `BaseURL` and segment references resolve against. `data:` URLs
    /// are decoded as with [`Self::new_from_data_url`].
    pub async fn new_from_url(url: &str) -> Result<Self, Error> {
        let client = Client::new();
        let parsed = match url.trim_start().starts_with("data:") {
            true => crate::DashManifest::new_from_data_url(url.trim())?,
            false => {
                let response = client
                    .get(url)
                    .send()
                    .await
                    .and_then(Response::error_for_status)
                    .map_err(Error::DashManifestRequest)?;
                let final_url = response.url().to_string();
                let dash_xml = response.text().await.map_err(Error::DashManifestRequest)?;
                crate::DashManifest::new(dash_xml)?.with_base_url(&final_url)?
            }
        };
        let mut manifest = Self::from(parsed).with_client(client);
        manifest.load_segment_indexes().await;
        Ok(manifest)
    }

    /// Fetches through `client` from now on, for its connection pool, proxy,
    /// timeouts or TLS settings.
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = OnceLock::from(client);
        self
    }

    /// Sets the absolute URL the manifest was loaded from. See
    /// [`crate::DashManifest::with_base_url`].
    pub fn with_base_url(self, url: &str) -> Result<Self, Error> {
        Ok(Self {
            manifest: self.manifest.with_base_url(url)?,
            ..self
        })
    }

    /// Fetches every `SegmentBase` index the manifest still needs.
    ///
    /// A representation whose index fails to load reports the failure when it
    /// is streamed; the others are unaffected. Call it again after
    /// [`Self::with_base_url`] when relative index URLs had nothing to resolve
    /// against.
    pub async fn load_segment_indexes(&mut self) {
        let client = self.client().clone();
        for request in self.manifest.index_requests() {
            let fragment = request.fragment();
            match Transport::get(&client, fragment, fragment.known_size()).await {
                Ok(body) => self.manifest.apply_index(&request, &body),
                Err(error) => self.manifest.fail_index(&request, error),
            }
        }
    }

    /// Opens a representation as a [`FragmentStream`]: a stream of its whole
    /// fragments, and an async reader of them. The initialization fragment is
    /// fetched before this returns.
    ///
    /// The representation is matched as by [`Self::representation`]. Call
    /// [`FragmentStream::start_eager_head`] to discover fragment sizes in the
    /// background, which can make byte seeking faster on servers that support
    /// `HEAD`.
    pub async fn stream(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<FragmentStream, Error> {
        let cache = FragmentCache::new(Arc::new(self.client().clone()));
        self.stream_with_cache(representation_id, Arc::new(cache))
            .await
    }

    /// Like [`Self::stream`], but fetches through `cache`.
    ///
    /// Pass clones of one [`FragmentCache`] to share pooled connections and
    /// downloaded fragments between streams, such as a stream rebuilt after a
    /// seek.
    pub async fn stream_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        cache: Arc<FragmentCache>,
    ) -> Result<FragmentStream, Error> {
        let id = representation_id.as_ref();
        let fragments = self.manifest.fragments(id)?;
        let timeline = self.manifest.media_timeline(id)?;
        FragmentStream::new_with_cache(fragments, Some(timeline), cache).await
    }

    /// Opens a representation as a [`FragmentStream`] that reads its
    /// initialization fragment, then its media from the fragment covering
    /// `time`, as a decoder starting there needs. Returns the stream and the
    /// start of that fragment, at or before `time`.
    ///
    /// The stream reports and seeks by the representation's media time. See
    /// [`FragmentStream::starting_at`].
    pub async fn stream_at(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
    ) -> Result<(FragmentStream, Duration), Error> {
        let cache = FragmentCache::new(Arc::new(self.client().clone()));
        self.stream_at_with_cache(representation_id, time, Arc::new(cache))
            .await
    }

    /// Like [`Self::stream_at`], but fetches through `cache`.
    pub async fn stream_at_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
        cache: Arc<FragmentCache>,
    ) -> Result<(FragmentStream, Duration), Error> {
        let id = representation_id.as_ref();
        let fragments = self.manifest.fragments(id)?;
        let timeline = self.manifest.media_timeline(id)?;
        FragmentStream::starting_at(fragments, timeline, time, cache).await
    }

    /// The URL the manifest was loaded from, if known.
    pub fn base_url(&self) -> Option<&str> {
        self.manifest.base_url()
    }

    /// Lists representations in document order. See
    /// [`crate::DashManifest::representations`].
    pub fn representations(&self) -> impl Iterator<Item = Representation<'_>> {
        self.manifest.representations()
    }

    /// Finds a representation by ID. See
    /// [`crate::DashManifest::representation`].
    pub fn representation(&self, representation_id: impl AsRef<str>) -> Option<Representation<'_>> {
        self.manifest.representation(representation_id)
    }

    /// Lists a representation's initialization fragment and its media
    /// fragments in timeline order. See [`crate::DashManifest::fragments`].
    pub fn fragments(&self, representation_id: impl AsRef<str>) -> Result<Fragments, Error> {
        self.manifest.fragments(representation_id)
    }

    /// Media-segment durations in timeline order, with the DASH timescale.
    pub fn media_timeline(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<MediaTimeline, Error> {
        self.manifest.media_timeline(representation_id)
    }

    /// MIME type of a representation, inherited from its adaptation set if absent.
    pub fn mime_type(&self, representation_id: impl AsRef<str>) -> Result<Option<&str>, Error> {
        self.manifest.mime_type(representation_id)
    }

    /// Common-encryption scheme protecting a representation, such as `cenc` or
    /// `cbcs`. See [`crate::DashManifest::protection_scheme`].
    pub fn protection_scheme(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<Option<String>, Error> {
        self.manifest.protection_scheme(representation_id)
    }

    /// The parsed manifest underneath.
    pub fn manifest(&self) -> &crate::DashManifest {
        &self.manifest
    }

    /// The parsed manifest underneath, without the client.
    pub fn into_inner(self) -> crate::DashManifest {
        self.manifest
    }

    /// The client fetches go through, made now if none was given.
    fn client(&self) -> &Client {
        self.client.get_or_init(Client::new)
    }
}

impl From<crate::DashManifest> for DashManifest {
    /// Wraps a parsed manifest, to fetch through a client of its own unless
    /// given one with [`DashManifest::with_client`].
    fn from(manifest: crate::DashManifest) -> Self {
        Self {
            manifest,
            client: OnceLock::new(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fragment::{Fragment, parse_byte_range};
    use bytes::Bytes;
    use futures_util::StreamExt;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::ops::RangeInclusive;
    use std::sync::Mutex;

    /// A test server's handler for one request, returning a status and a body.
    type Respond = dyn Fn(&str, Option<RangeInclusive<u64>>) -> (u16, Vec<u8>) + Send + Sync;

    /// Requests a test server has answered, by path.
    pub(crate) type Counts = Arc<Mutex<HashMap<String, usize>>>;

    /// Serves HTTP on a local port, one thread per connection, and counts the
    /// requests for each path. Returns the base URL and the counts.
    pub(crate) fn serve(
        respond: impl Fn(&str, Option<RangeInclusive<u64>>) -> (u16, Vec<u8>) + Send + Sync + 'static,
    ) -> (String, Counts) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let respond: Arc<Respond> = Arc::new(respond);
        let counts = Counts::default();
        let served = counts.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (respond, served) = (respond.clone(), served.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.unwrap());
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let path = request_line.split(' ').nth(1).unwrap().to_owned();
                    let mut range = None;
                    let mut header = String::new();
                    while reader.read_line(&mut header).unwrap() > 2 {
                        if let Some(value) =
                            header.to_ascii_lowercase().strip_prefix("range: bytes=")
                        {
                            range = Some(parse_byte_range(value).unwrap());
                        }
                        header.clear();
                    }
                    *served.lock().unwrap().entry(path.clone()).or_insert(0) += 1;
                    let (status, body) = respond(&path, range);
                    let head = match status {
                        302 => format!(
                            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            String::from_utf8(body.clone()).unwrap()
                        ),
                        _ => format!(
                            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        ),
                    };
                    let stream = reader.get_mut();
                    stream.write_all(head.as_bytes()).unwrap();
                    if status != 302 {
                        stream.write_all(&body).unwrap();
                    }
                });
            }
        });
        (format!("http://{address}"), counts)
    }

    /// Serves each path's name as its body.
    pub(crate) fn named() -> impl Fn(&str, Option<RangeInclusive<u64>>) -> (u16, Vec<u8>) {
        |path, _| (200, path.trim_start_matches('/').as_bytes().to_vec())
    }

    /// Every item of `stream`, as text.
    pub(crate) async fn texts(stream: &mut FragmentStream) -> Vec<String> {
        stream
            .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
            .collect()
            .await
    }

    fn index_len(index_range: &str) -> usize {
        let range = parse_byte_range(index_range).unwrap();
        (*range.end() - *range.start() + 1) as usize
    }

    #[tokio::test]
    async fn new_from_url_follows_redirects_and_loads_indexes() {
        let (file, index_range) = crate::manifest::segment_base::tests::segment_base_file();
        let xml = format!(
            r#"<MPD><Period><AdaptationSet mimeType="audio/mp4"><Representation id="FLAC">
                <BaseURL>track.mp4</BaseURL><SegmentBase indexRange="{index_range}"/>
            </Representation></AdaptationSet></Period></MPD>"#
        );
        let served = file.clone();
        let (base, _) = serve(move |path, range| match (path, range) {
            ("/start", _) => (302, b"/audio/manifest.mpd".to_vec()),
            ("/audio/manifest.mpd", _) => (200, xml.clone().into_bytes()),
            ("/audio/track.mp4", Some(range)) => (
                206,
                served[*range.start() as usize..=*range.end() as usize].to_vec(),
            ),
            _ => (404, Vec::new()),
        });

        let manifest = DashManifest::new_from_url(&format!("{base}/start"))
            .await
            .unwrap();
        assert_eq!(
            manifest.base_url(),
            Some(format!("{base}/audio/manifest.mpd").as_str())
        );
        let fragments = manifest.fragments("FLAC").unwrap();
        assert_eq!(fragments.media.len(), 2);
        assert_eq!(
            fragments.init,
            Fragment::with_range(format!("{base}/audio/track.mp4"), 0..=837)
        );
        assert_eq!(
            manifest.media_timeline("FLAC").unwrap().media_durations,
            [4096, 1024]
        );

        // The stream reads the init, then the media past the index.
        let (init, media) = (&file[..838], &file[838 + index_len(&index_range)..]);
        let items: Vec<Bytes> = manifest
            .stream("FLAC")
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect()
            .await;
        assert_eq!(items, [init, &media[..100], &media[100..]]);
    }

    #[tokio::test]
    async fn stream_opens_a_representation_through_the_given_client() {
        let (base, _) = serve(named());
        let xml = r#"<MPD><Period><AdaptationSet mimeType="audio/mp4">
            <Representation id="FLAC,24bit">
                <SegmentTemplate initialization="init" media="$Number$">
                    <SegmentTimeline><S d="1" r="2"/></SegmentTimeline>
                </SegmentTemplate>
            </Representation>
        </AdaptationSet></Period></MPD>"#;
        let manifest = DashManifest::new(xml)
            .unwrap()
            .with_client(Client::new())
            .with_base_url(&format!("{base}/manifest.mpd"))
            .unwrap();

        let mut stream = manifest.stream("FLAC").await.unwrap();
        assert_eq!(texts(&mut stream).await, ["init", "1", "2", "3"]);

        let (mut stream, start) = manifest
            .stream_at("FLAC", Duration::from_millis(1_500))
            .await
            .unwrap();
        assert_eq!(start, Duration::from_secs(1));
        assert_eq!(texts(&mut stream).await, ["init", "2", "3"]);
        assert!(matches!(
            manifest.stream("AAC").await,
            Err(Error::DashManifestMissingRepresentation(id)) if id == "AAC"
        ));
    }
}
