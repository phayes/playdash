//! The async counterparts of [`playdash::fragment`](crate::fragment): the
//! [`FragmentStream`] that downloads a representation's fragments, and the
//! [`FragmentCache`] and [`Transport`] it fetches through.

mod cache;
mod manifest;
mod stream;
mod transport;

pub use cache::FragmentCache;
pub use stream::FragmentStream;
pub use transport::Transport;
