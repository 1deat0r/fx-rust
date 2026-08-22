//! Phase 5 TUI — a full-screen terminal UI for fxrs (port of upstream
//! `src/ui/**`, ~150 files): render engine, transcript runtime, composer
//! (built on `input_composer`), theme, and the app event loop.
//!
//! The TUI is a full-screen application: it enters the alternate screen,
//! enables raw mode, and paints a composer screen (header + transcript +
//! input line + footer) with incremental cell-diff rendering. The agent loop
//! runs as a background task and streams events into the transcript through a
//! `TuiHuman` sink, so rendering never blocks and the input line stays live.

pub mod app;
pub mod composer;
pub mod keys;
pub mod screen;
pub mod theme;
pub mod transcript;
pub mod widgets;

pub use app::run_tui;
