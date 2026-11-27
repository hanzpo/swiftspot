//! SwiftSpot's Rust playback, Spotify API, and library core.
//!
//! Exposed both as a Rust library (`rlib`) for unit tests and as a static
//! library (`libswiftspot.a`) with a C FFI (`crate::ffi`) embedded directly
//! into the native macOS SwiftUI application.

pub mod api;
pub mod app;
pub mod auth;
pub mod backend;
pub mod credentials;
pub mod eq;
pub mod ffi;
pub mod file_writer;
pub mod history;
pub mod home_cache;
pub mod http;
pub mod i18n;
pub mod images;
pub mod jam;
pub mod karaoke;
pub mod liked;
pub mod limiter;
pub mod link;
pub mod lyrics;
pub mod model;
pub mod opener;
pub mod pathfinder;
pub mod paths;
pub mod player;
pub mod playlist_cover;
pub mod resample;
pub mod server;
pub mod session_reads;
pub mod settings;
pub mod sink;
pub mod util;
pub mod words;
pub mod zeroconf;
