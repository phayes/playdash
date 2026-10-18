//! The segment store shared by the blocking and async caches: what is known about each
//! segment, and what a fetched body goes through before readers see it. Nothing here does
//! I/O.

use super::SegmentLocation;
#[cfg(feature = "encryption")]
use crate::cenc::{self, ContentKeys, SegmentRole};
use crate::error::Error;
use bytes::Bytes;
#[cfg(feature = "encryption")]
use bytes::BytesMut;
use std::collections::HashMap;
#[cfg(feature = "encryption")]
use std::sync::RwLock;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

/// A segment's data after it's been fetched, shared between streams.
#[derive(Debug)]
pub(crate) struct SegmentData {
    pub(crate) location: SegmentLocation,
    held: Mutex<Held>,
    /// Length of the fetched body, kept after the body is forgotten.
    fetched_size: OnceLock<u64>,
    /// Length from HEAD, before the body arrives.
    pub(crate) probed_size: OnceLock<u64>,
    /// Held across a blocking GET, so a second stream waits for that download
    /// instead of starting its own.
    #[cfg(feature = "sync")]
    pub(crate) blocking_fetch: Mutex<()>,
    /// Held across a blocking HEAD, as `blocking_fetch` is for GETs.
    #[cfg(feature = "sync")]
    pub(crate) blocking_probe: Mutex<()>,
    /// Held across an async GET, as `blocking_fetch` is for blocking ones.
    #[cfg(feature = "async")]
    pub(crate) async_fetch: futures_util::lock::Mutex<()>,
    /// Held across an async HEAD.
    #[cfg(feature = "async")]
    pub(crate) async_probe: futures_util::lock::Mutex<()>,
    /// This segment's role in decryption, which tells a keyed cache how to
    /// turn its fetched bytes into plaintext. A stream sets it before the first GET of the segment.
    #[cfg(feature = "encryption")]
    pub(crate) role: OnceLock<SegmentRole>,
}

/// A segment's body and the streams that want it kept, changed together so
/// a release cannot race a download.
#[derive(Debug, Default)]
struct Held {
    body: Option<Bytes>,
    /// Number of streams that have this segment in their retention window.
    holds: usize,
}

impl SegmentData {
    pub(crate) fn new(location: SegmentLocation) -> Self {
        Self {
            location,
            held: Mutex::default(),
            fetched_size: OnceLock::new(),
            probed_size: OnceLock::new(),
            #[cfg(feature = "sync")]
            blocking_fetch: Mutex::new(()),
            #[cfg(feature = "sync")]
            blocking_probe: Mutex::new(()),
            #[cfg(feature = "async")]
            async_fetch: futures_util::lock::Mutex::new(()),
            #[cfg(feature = "async")]
            async_probe: futures_util::lock::Mutex::new(()),
            #[cfg(feature = "encryption")]
            role: OnceLock::new(),
        }
    }

    /// The body, if it has been fetched and not forgotten.
    pub(crate) fn body(&self) -> Option<Bytes> {
        self.lock_held().body.clone()
    }

    /// Whether the body is held now.
    pub(crate) fn has_body(&self) -> bool {
        self.lock_held().body.is_some()
    }

    /// Keeps `body`, or the body another stream stored first.
    fn keep(&self, body: Bytes) -> Bytes {
        let mut held = self.lock_held();
        let body = held.body.get_or_insert(body).clone();
        self.fetched_size.get_or_init(|| body.len() as u64);
        body
    }

    /// Counts one more stream that wants this segment kept.
    fn acquire(&self) {
        self.lock_held().holds += 1;
    }

    /// Counts one stream fewer, and with `forget` drops the body once no
    /// stream wants it. Returns whether a body was dropped.
    fn release(&self, forget: bool) -> bool {
        let mut held = self.lock_held();
        held.holds = held.holds.saturating_sub(1);
        forget && held.holds == 0 && held.body.take().is_some()
    }

    /// Drops the body if no stream wants it. Returns whether one was dropped.
    fn forget_if_unheld(&self) -> bool {
        let mut held = self.lock_held();
        held.holds == 0 && held.body.take().is_some()
    }

    #[cfg(test)]
    pub(crate) fn set_body(&self, body: &[u8]) {
        self.keep(Bytes::copy_from_slice(body));
    }

    /// Byte length from the range, else the fetched body, else a probe.
    pub(crate) fn size(&self) -> Option<u64> {
        self.location
            .known_size()
            .or_else(|| self.fetched_size.get().copied())
            .or_else(|| self.probed_size.get().copied())
    }

    /// The length of the fetched body, once there has been one.
    pub(crate) fn fetched_size(&self) -> Option<u64> {
        self.fetched_size.get().copied()
    }

    fn lock_held(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// Subscriber to a [`Store`]; see [`Store::subscribe`].
pub(crate) trait CacheListener: Send + Sync {
    /// `data`'s body or size has just changed.
    fn segment_changed(&self, data: &SegmentData);
}

/// The segments a cache knows about, and the content keys it decrypts with.
#[derive(Default)]
pub(crate) struct Store {
    segments: Mutex<HashMap<SegmentLocation, Arc<SegmentData>>>,
    /// Called after a segment's body or size changes. Held weakly, so a
    /// listener lives as long as its subscriber keeps it.
    listeners: Mutex<Vec<Weak<dyn CacheListener>>>,
    /// Set once, before any segment is known; later only merged into. Replaced as a whole,
    /// so a reader's clone stays consistent.
    #[cfg(feature = "encryption")]
    keys: RwLock<Option<Arc<ContentKeys>>>,
}

impl Store {
    #[cfg(feature = "encryption")]
    pub(crate) fn set_keys(&mut self, keys: ContentKeys) {
        *self
            .keys
            .get_mut()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(keys));
    }

    /// Merges `keys` into the store's keys. A store without keys takes them only if it
    /// knows no segments yet: one that does may hold bodies it kept undecrypted.
    #[cfg(feature = "encryption")]
    pub(crate) fn add_keys(&self, keys: ContentKeys) -> Result<(), Error> {
        // Held throughout, so no stream can start using the store meanwhile.
        let segments = self.lock();
        let mut current = self.keys.write().unwrap_or_else(|error| error.into_inner());
        match current.as_deref() {
            Some(existing) => {
                let mut merged = existing.clone();
                merged.merge(keys);
                *current = Some(Arc::new(merged));
            }
            None if segments.is_empty() => *current = Some(Arc::new(keys)),
            None => {
                return Err(Error::Initialization(
                    "content keys cannot be added to a cache that has been used without \
                     them; give it the keys with SegmentCache::with_keys when creating it"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// The keys fetched bodies are decrypted with, if any.
    #[cfg(feature = "encryption")]
    fn keys(&self) -> Option<Arc<ContentKeys>> {
        self.keys
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Whether fetched bodies are decrypted before they are kept.
    pub(crate) fn decrypts(&self) -> bool {
        #[cfg(feature = "encryption")]
        return self.keys().is_some();
        #[cfg(not(feature = "encryption"))]
        false
    }

    /// The body of the segment at `location`, if it is held.
    pub(crate) fn cached(&self, location: &SegmentLocation) -> Option<Bytes> {
        self.lock().get(location)?.body()
    }

    /// The byte length of the segment at `location`, if its range, a HEAD or
    /// a GET has established it.
    pub(crate) fn known_size(&self, location: &SegmentLocation) -> Option<u64> {
        location
            .known_size()
            .or_else(|| self.lock().get(location)?.size())
    }

    /// Number of segments the store knows about.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// The shared entry for the segment at `location`, created empty if new.
    pub(crate) fn data(&self, location: SegmentLocation) -> Arc<SegmentData> {
        self.lock()
            .entry(location)
            .or_insert_with_key(|location| Arc::new(SegmentData::new(location.clone())))
            .clone()
    }

    /// Tells `listener` after any segment's body or size changes, on the
    /// thread that changed it, until `listener` is dropped.
    pub(crate) fn subscribe(&self, listener: Weak<dyn CacheListener>) {
        self.listeners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(listener);
    }

    /// Checks a freshly fetched `body` of `data`, decrypts it if the store
    /// has keys, and keeps it. A ranged body of the wrong length is an error
    /// and is not kept.
    pub(crate) fn accept(&self, data: &SegmentData, body: Bytes) -> Result<Bytes, Error> {
        let location = &data.location;
        if let Some(expected) = location.known_size()
            && body.len() as u64 != expected
        {
            return Err(Error::SegmentLength {
                segment: location.to_string(),
                expected,
                actual: body.len() as u64,
            });
        }
        #[cfg(feature = "encryption")]
        let body = self.decrypt(data, body)?;
        let body = data.keep(body);
        self.notify(data);
        Ok(body)
    }

    /// Records a HEAD length for `data`, unless its size is already known.
    pub(crate) fn accept_probe(&self, data: &SegmentData, size: u64) {
        data.probed_size.get_or_init(|| size);
        self.notify(data);
    }

    /// Records that a stream wants each of `segments` kept.
    pub(crate) fn acquire(&self, segments: &[Arc<SegmentData>]) {
        for data in segments {
            data.acquire();
        }
    }

    /// Records that a stream no longer wants each of `segments` kept, and
    /// with `forget` drops the bodies no stream wants any more.
    pub(crate) fn release(&self, segments: &[Arc<SegmentData>], forget: bool) {
        for data in segments {
            if data.release(forget) {
                self.notify(data);
            }
        }
    }

    /// Drops `data`'s body if no stream wants it, as after a download that
    /// landed outside every window.
    pub(crate) fn forget_if_unheld(&self, data: &SegmentData) {
        if data.forget_if_unheld() {
            self.notify(data);
        }
    }

    /// Marks `init` as an initialization segment, so a keyed store clears
    /// its protected sample entries when it is fetched.
    #[cfg(feature = "encryption")]
    pub(crate) fn assign_init_role(&self, init: &SegmentData) {
        let _ = init.role.set(SegmentRole::Init);
    }

    /// Tags `media` with the protection parsed from the already fetched and
    /// cleared body of `init`. Fails before any media GET if a protected
    /// track's KID has no key.
    #[cfg(feature = "encryption")]
    pub(crate) fn assign_media_roles<'a>(
        &self,
        init: &SegmentData,
        media: impl IntoIterator<Item = &'a SegmentData>,
    ) -> Result<(), Error> {
        let Some(keys) = self.keys() else {
            return Ok(());
        };
        let body = init.body().expect("init fetched before media roles");
        let protection = cenc::protection(&body).map_err(|problem| problem.at(&init.location))?;
        if let Some(track) = protection
            .tracks
            .iter()
            .find(|track| !keys.contains(&track.kid))
        {
            return Err(Error::MissingContentKey(cenc::hex(&track.kid)));
        }
        let protection = Arc::new(protection);
        for data in media {
            // Already set if another stream of this store got here first.
            let _ = data.role.set(SegmentRole::Media(protection.clone()));
        }
        Ok(())
    }

    /// Decrypts `body` according to `data`'s role. Same length out as in, so
    /// ranges, HEAD sizes and byte seeks are unaffected.
    #[cfg(feature = "encryption")]
    fn decrypt(&self, data: &SegmentData, body: Bytes) -> Result<Bytes, Error> {
        let Some(keys) = self.keys() else {
            return Ok(body);
        };
        let role = match data.role.get() {
            Some(SegmentRole::Media(protection)) if protection.tracks.is_empty() => {
                return Ok(body);
            }
            Some(role) => role,
            None => {
                debug_assert!(
                    false,
                    "keyed cache fetched {} before a stream set its role",
                    data.location
                );
                return Ok(body);
            }
        };
        // A unique body, as transports hand over, converts without a copy.
        let mut bytes = BytesMut::from(body);
        match role {
            SegmentRole::Init => cenc::clear_sample_entries(&mut bytes),
            SegmentRole::Media(protection) => {
                let body_start = data
                    .location
                    .range
                    .as_ref()
                    .map_or(0, |range| *range.start());
                cenc::decrypt_segment(&mut bytes, body_start, protection, &keys)
            }
        }
        .map_err(|problem| problem.at(&data.location))?;
        Ok(bytes.freeze())
    }

    fn notify(&self, data: &SegmentData) {
        let listeners: Vec<_> = {
            let mut listeners = self
                .listeners
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            listeners.retain(|listener| listener.strong_count() > 0);
            listeners.iter().filter_map(Weak::upgrade).collect()
        };
        for listener in listeners {
            listener.segment_changed(data);
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SegmentLocation, Arc<SegmentData>>> {
        self.segments
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}
