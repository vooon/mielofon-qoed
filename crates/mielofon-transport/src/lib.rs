//! Control-plane transport for mielofon.
//!
//! One **session** per agent↔controller pair and per member→member
//! direction, carried over mTLS QUIC. Either side can open a **bidirectional stream**
//! on a session: the opener writes one request frame and closes its send
//! side, the acceptor answers with one response frame and closes. A request
//! is a stream, so correlation, cancellation and deadlines are the stream's
//! and no job queue is needed. The controller never dials an agent (spokes
//! sit behind NAT); once an agent has dialled in, the controller opens
//! streams on that session for every command.
//!
//! Frames are length-prefixed JSON of the `mielofon-proto` types. The
//! carrier is abstracted by [`Session`] so a second one (TLS over TCP with a
//! stream multiplexer) can be plugged in without touching the callers.
//!
//! Control never rides the mesh it costs: endpoints are underlay addresses.

pub mod frame;
pub mod quic;
pub mod resolve;
pub mod session;

pub use frame::{read_frame, write_frame, FrameError, MAX_FRAME};
#[cfg(any(test, feature = "testpki"))]
pub use quic::testpki;
pub use quic::{Dialer, Listener, QuicSession};
pub use resolve::{parse_endpoint, resolve_all, Endpoint};
pub use session::{request, respond, BiStream, BoxFuture, Session};

/// ALPN of the control protocol; bumped when the frame contract changes.
pub const ALPN: &[u8] = b"mielofon/1";
