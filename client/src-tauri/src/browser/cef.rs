//! The takeover's native half: the overlay that draws the server's page and
//! the gestures it sends back.
//!
//! The engine is not here any more. forge-server runs the CEF host and the
//! driver; this process receives encoded frames over the socket and paints
//! them, so what is left is a view, a decoder and the input mapping.

pub mod input;
pub mod surface;
