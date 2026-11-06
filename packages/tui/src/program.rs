//! Glue between [`App`] and `bmux_tui_runtime`'s program model.

use std::io::{Stdout, stdout};
use std::time::Duration;

use bmux_tui::crossterm::{CrosstermTerminalGuard, terminal_size};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::terminal::Terminal;
use bmux_tui_runtime::{
    MessageKey, Program, Runtime, RuntimeConfig, RuntimeError, RuntimeEvent, TerminalInput,
    TerminalPresenter, Update,
};

use crate::app::{App, Handled, TuiOptions};
use crate::backend::{Backend, BackendEvent, EventSink};
use crate::render::render;

/// Error returned by [`run`].
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// Terminal I/O failed.
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The async runtime could not be created.
    #[error("failed to start the async runtime: {0}")]
    Runtime(std::io::Error),
}

/// What the UI left behind when it exited.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiExit {
    /// Paths still marked at exit (top-level only).
    pub marked: Vec<std::path::PathBuf>,
}

/// Runtime message type.
#[derive(Debug)]
pub enum Msg {
    /// From the backend.
    Backend(BackendEvent),
    /// Animation tick.
    Tick,
    /// Terminal input failure.
    InputError(std::io::Error),
}

/// The [`Program`] driving an [`App`].
pub struct BcleanProgram<B: Backend> {
    /// Application state.
    pub app: App<B>,
}

impl<B: Backend> Program for BcleanProgram<B> {
    type Message = Msg;
    type Error = std::io::Error;

    fn update(&mut self, event: RuntimeEvent<Msg>) -> Result<Update<Msg>, std::io::Error> {
        Ok(match event {
            RuntimeEvent::Terminal(event) => match self.app.handle_event(&event) {
                Handled::Quit => Update::exit(),
                Handled::Redraw => Update::redraw(),
                Handled::Ignored => Update::none(),
            },
            RuntimeEvent::Message(Msg::Backend(event)) => {
                self.app.apply(event);
                Update::redraw()
            }
            RuntimeEvent::Message(Msg::Tick) => {
                if self.app.on_tick() {
                    Update::redraw()
                } else {
                    Update::none()
                }
            }
            RuntimeEvent::Message(Msg::InputError(error)) => return Err(error),
            RuntimeEvent::Timer(_) => Update::none(),
        })
    }
}

fn render_program<B: Backend>(program: &mut BcleanProgram<B>, cx: &mut PaintCx<'_, '_>) {
    render(&mut program.app, cx);
}

/// Runs the interactive UI until the user quits.
///
/// Takes over the terminal (raw mode + alternate screen) and restores it on exit, including on
/// errors.
///
/// # Errors
///
/// * [`TuiError::Runtime`] if the async runtime cannot be created
/// * [`TuiError::Io`] if the terminal cannot be initialized, drawn, or restored, or input fails
pub fn run<B: Backend>(backend: B, options: TuiOptions) -> Result<TuiExit, TuiError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(TuiError::Runtime)?;
    rt.block_on(run_async(backend, options))
}

async fn run_async<B: Backend>(backend: B, options: TuiOptions) -> Result<TuiExit, TuiError> {
    let mut guard = CrosstermTerminalGuard::enter(stdout())?;
    let result = run_in_terminal(&mut guard, backend, options).await;
    let restoration = guard.leave().map(|_: Stdout| ());
    match (result, restoration) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(exit), Ok(())) => Ok(exit),
    }
}

async fn run_in_terminal<B: Backend>(
    guard: &mut CrosstermTerminalGuard<Stdout>,
    backend: B,
    options: TuiOptions,
) -> Result<TuiExit, TuiError> {
    let writer = guard
        .writer_mut()
        .ok_or_else(|| std::io::Error::other("terminal guard does not own stdout"))?;
    let size = terminal_size()?;
    let terminal = Terminal::new(writer, Rect::new(0, 0, size.width, size.height));
    let presenter = TerminalPresenter::new(terminal, render_program::<B>);

    let frame_interval = options.frame_interval;
    let (sink, mut events) = EventSink::channel();
    let program = BcleanProgram {
        app: App::new(backend, options, sink),
    };
    let (runtime, handle) = Runtime::new(
        program,
        presenter,
        RuntimeConfig {
            frame_interval,
            ..RuntimeConfig::default()
        },
    );

    // Backend events → runtime (bounded admission applies backpressure to this task only).
    let forward = handle.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if forward.send(Msg::Backend(event)).await.is_err() {
                break;
            }
        }
    });
    // Animation ticks; coalesced so a slow frame never builds a backlog.
    let tick_handle = handle.clone();
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(120));
        loop {
            interval.tick().await;
            if tick_handle
                .send_latest(MessageKey::new("tick"), Msg::Tick)
                .is_err()
            {
                break;
            }
        }
    });

    let mut input = TerminalInput::start::<BcleanProgram<B>>(handle, Msg::InputError);
    let result = runtime.run().await;
    input.shutdown().await;
    forwarder.abort();
    ticker.abort();

    match result {
        Ok(output) => {
            let marked =
                crate::listing::top_level_paths(output.program.app.marked().iter().cloned());
            Ok(TuiExit { marked })
        }
        Err(
            RuntimeError::Program {
                error,
                cleanup_error,
                ..
            }
            | RuntimeError::Presenter {
                error,
                cleanup_error,
                ..
            },
        ) => Err(match cleanup_error {
            Some(cleanup) => std::io::Error::other(format!(
                "{error}; pointer restoration also failed: {cleanup}"
            )),
            None => error,
        }
        .into()),
    }
}
