#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Interactive bclean terminal UI built on bmux TUI.
//!
//! An ncdu-style browser with bclean's extras: classification badges and scores, lenses
//! (ephemeral / stale / reclaimable across the whole tree), marking with a plan → confirm →
//! execute → report flow, an explain panel, and a dashboard of volumes and alerts.
//!
//! The UI talks to the rest of bclean only through the [`Backend`] trait (see [`backend`]),
//! which the integration layer implements for local mode and daemon (IPC) mode. Start it with
//! [`run`]:
//!
//! ```no_run
//! use bclean_tree::{EntryMeta, Tree};
//! use bclean_tui::{InMemoryBackend, TuiOptions, run};
//!
//! let tree = Tree::new("/tmp", EntryMeta::dir());
//! let exit = run(InMemoryBackend::new(tree), TuiOptions::default())?;
//! println!("{} paths still marked", exit.marked.len());
//! # Ok::<(), bclean_tui::TuiError>(())
//! ```
//!
//! Architecture: [`App`] holds all state and handles input and backend events without touching
//! the terminal; `render` draws it onto a bmux `PaintCx`; `program` adapts it to
//! `bmux_tui_runtime`'s `Program` model. Rendering is therefore testable headlessly by painting
//! into a `bmux_tui::buffer::Buffer`.

mod app;
pub mod backend;
pub mod dashboard;
mod format;
pub mod listing;
mod memory;
mod program;
mod render;
pub mod widgets;

pub use app::{App, ExecState, Handled, MessageLevel, Modal, TuiOptions, View};
pub use backend::{
    ActionOverride, Backend, BackendEvent, BackendMode, BackendStatus, CleanPlan, CleanReport,
    EventReceiver, EventSink, ExecutionEvent, ItemOutcome, PlanItem, PlanRequest, ScanProgress,
};
pub use listing::{Lens, SortBy, SortSpec};
pub use memory::InMemoryBackend;
pub use program::{BcleanProgram, Msg, TuiError, TuiExit, run};

/// Renders `app` into an off-screen buffer of `width` × `height` cells.
///
/// Used by snapshot tests and handy for debugging layouts without a terminal.
#[must_use]
pub fn render_to_buffer<B: Backend>(
    app: &mut App<B>,
    width: u16,
    height: u16,
) -> bmux_tui::buffer::Buffer {
    use bmux_tui::geometry::Rect;
    let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
    render::render(app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    buffer
}

/// Renders `app` to plain text lines (symbols only), for snapshot tests.
#[must_use]
pub fn render_to_text<B: Backend>(app: &mut App<B>, width: u16, height: u16) -> Vec<String> {
    let buffer = render_to_buffer(app, width, height);
    (0..height)
        .map(|y| {
            buffer
                .row_symbols(y)
                .unwrap_or_default()
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[cfg(test)]
mod tests;
