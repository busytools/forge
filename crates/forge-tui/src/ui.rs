//! Render layer.
//!
//! Three submodules: `layout` lays out the three panels and overlays the
//! permission modal, `conversation` renders one transcript line, and
//! `permission_modal` paints the overlay box.

pub mod conversation;
pub mod layout;
pub mod permission_modal;

pub use layout::render;
