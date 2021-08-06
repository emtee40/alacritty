//! Terminal window context.

use std::error::Error;
use std::mem;
#[cfg(not(any(target_os = "macos", windows)))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossfont::Size;
use glutin::event::{Event as GlutinEvent, ModifiersState, WindowEvent};
use glutin::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use log::info;

use alacritty_terminal::event::Event as TerminalEvent;
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::tty;

use crate::clipboard::Clipboard;
use crate::config::Config;
use crate::display::Display;
use crate::event::{ActionContext, Event, EventProxy, EventType, Mouse, SearchState};
use crate::input;
use crate::message_bar::MessageBuffer;
use crate::scheduler::Scheduler;

/// Event context for one individual Alacritty window.
pub struct WindowContext {
    pub event_queue: Vec<GlutinEvent<'static, Event>>,
    pub terminal: Arc<FairMutex<Term<EventProxy>>>,
    pub message_buffer: MessageBuffer,
    pub modifiers: ModifiersState,
    pub search_state: SearchState,
    pub received_count: usize,
    pub suppress_chars: bool,
    pub display: Display,
    pub font_size: Size,
    pub mouse: Mouse,
    pub notifier: Notifier,
    pub dirty: bool,
}

impl WindowContext {
    /// Create a new terminal window context.
    pub fn new(
        config: &Config,
        window_event_loop: &EventLoopWindowTarget<Event>,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        // Create a display.
        //
        // The display manages a window and can draw the terminal.
        let display = Display::new(config, window_event_loop)?;

        info!(
            "PTY dimensions: {:?} x {:?}",
            display.size_info.screen_lines(),
            display.size_info.columns()
        );

        let event_proxy = EventProxy::new(proxy, display.window.id());

        // Create the terminal.
        //
        // This object contains all of the state about what's being displayed. It's
        // wrapped in a clonable mutex since both the I/O loop and display need to
        // access it.
        let terminal = Term::new(config, display.size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        // Create the PTY.
        //
        // The PTY forks a process to run the shell on the slave side of the
        // pseudoterminal. A file descriptor for the master side is retained for
        // reading/writing to the shell.
        let pty = tty::new(config, &display.size_info, display.window.x11_window_id());

        // Create the pseudoterminal I/O loop.
        //
        // PTY I/O is ran on another thread as to not occupy cycles used by the
        // renderer and input processing. Note that access to the terminal state is
        // synchronized since the I/O loop updates the state, and the display
        // consumes it periodically.
        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            config.hold,
            config.ui_config.debug.ref_test,
        );

        // The event loop channel allows write requests from the event processor
        // to be sent to the pty loop and ultimately written to the pty.
        let loop_tx = event_loop.channel();

        // Kick off the I/O thread.
        let _io_thread = event_loop.spawn();

        // Start cursor blinking, in case `Focused` isn't sent on startup.
        if config.cursor.style().blinking {
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        // Create context for the Alacritty window.
        Ok(WindowContext {
            font_size: config.ui_config.font.size(),
            notifier: Notifier(loop_tx),
            terminal,
            display,
            suppress_chars: Default::default(),
            message_buffer: Default::default(),
            received_count: Default::default(),
            search_state: Default::default(),
            event_queue: Default::default(),
            modifiers: Default::default(),
            mouse: Default::default(),
            dirty: Default::default(),
        })
    }

    /// Update the terminal window to the latest config.
    pub fn update_config(&mut self, old_config: &Config, config: &Config) {
        self.display.update_config(config);
        self.terminal.lock().update_config(config);

        // Reload cursor if its thickness has changed.
        if (old_config.cursor.thickness() - config.cursor.thickness()).abs() > f32::EPSILON {
            self.display.pending_update.set_cursor_dirty();
        }

        if old_config.ui_config.font != config.ui_config.font {
            // Do not update font size if it has been changed at runtime.
            if self.font_size == old_config.ui_config.font.size() {
                self.font_size = config.ui_config.font.size();
            }

            let font = config.ui_config.font.clone().with_size(self.font_size);
            self.display.pending_update.set_font(font);
        }

        // Update display if padding options were changed.
        let window_config = &old_config.ui_config.window;
        if window_config.padding(1.) != config.ui_config.window.padding(1.)
            || window_config.dynamic_padding != config.ui_config.window.dynamic_padding
        {
            self.display.pending_update.dirty = true;
        }

        // Live title reload.
        if !config.ui_config.window.dynamic_title
            || old_config.ui_config.window.title != config.ui_config.window.title
        {
            self.display.window.set_title(&config.ui_config.window.title);
        }

        #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
        self.display.window.set_wayland_theme(&config.ui_config.colors);

        // Set subpixel anti-aliasing.
        #[cfg(target_os = "macos")]
        crossfont::set_font_smoothing(config.ui_config.font.use_thin_strokes);

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        self.display.window.set_has_shadow(config.ui_config.window_opacity() >= 1.0);

        // Update hint keys.
        self.display.hint_state.update_alphabet(config.ui_config.hints.alphabet());

        // Update cursor blinking.
        let event = Event::new(TerminalEvent::CursorBlinkingChange.into(), None);
        self.event_queue.push(event.into());

        self.dirty = true;
    }

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        event_loop: &EventLoopWindowTarget<Event>,
        config: &mut Config,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: GlutinEvent<'_, Event>,
    ) {
        match event {
            GlutinEvent::RedrawEventsCleared => (),
            // Remap DPR change event to remove the lifetime.
            GlutinEvent::WindowEvent {
                event: WindowEvent::ScaleFactorChanged { scale_factor, new_inner_size },
                window_id,
            } => {
                let size = (new_inner_size.width, new_inner_size.height);
                let event = Event::new(EventType::DprChanged(scale_factor, size), Some(window_id));
                self.event_queue.push(event.into());
                return;
            },
            // Transmute to extend lifetime, which exists only for `ScaleFactorChanged` event.
            // Since we remap that event to remove the lifetime, this is safe.
            event => unsafe {
                self.event_queue.push(mem::transmute(event));
                return;
            },
        }

        if self.event_queue_empty() {
            return;
        }

        let mut terminal = self.terminal.lock();

        let old_is_searching = self.search_state.history_index.is_some();

        let context = ActionContext {
            message_buffer: &mut self.message_buffer,
            received_count: &mut self.received_count,
            suppress_chars: &mut self.suppress_chars,
            search_state: &mut self.search_state,
            modifiers: &mut self.modifiers,
            font_size: &mut self.font_size,
            notifier: &mut self.notifier,
            display: &mut self.display,
            mouse: &mut self.mouse,
            dirty: &mut self.dirty,
            terminal: &mut terminal,
            event_loop,
            clipboard,
            scheduler,
            config,
        };
        let mut processor = input::Processor::new(context);

        for event in self.event_queue.drain(..) {
            processor.handle_event(event);
        }

        // Process DisplayUpdate events.
        if self.display.pending_update.dirty {
            Self::submit_display_update(
                &mut terminal,
                &mut self.display,
                &mut self.notifier,
                &self.message_buffer,
                &self.search_state,
                old_is_searching,
                config,
            );
        }

        if self.dirty || self.mouse.hint_highlight_dirty {
            self.dirty |= self.display.update_highlighted_hints(
                &terminal,
                config,
                &self.mouse,
                self.modifiers,
            );
            self.mouse.hint_highlight_dirty = false;
        }

        // Skip rendering on Wayland until we get frame event from compositor.
        #[cfg(not(any(target_os = "macos", windows)))]
        if !self.display.is_x11 && !self.display.window.should_draw.load(Ordering::Relaxed) {
            return;
        }

        if self.dirty {
            self.dirty = false;

            // Request immediate re-draw if visual bell animation is not finished yet.
            if !self.display.visual_bell.completed() {
                self.display.window.request_redraw();
            }

            // Redraw screen.
            self.display.draw(terminal, &self.message_buffer, config, &self.search_state);
        }
    }

    /// Submit the pending changes to the `Display`.
    fn submit_display_update(
        terminal: &mut Term<EventProxy>,
        display: &mut Display,
        notifier: &mut Notifier,
        message_buffer: &MessageBuffer,
        search_state: &SearchState,
        old_is_searching: bool,
        config: &Config,
    ) {
        // Compute cursor positions before resize.
        let num_lines = terminal.screen_lines();
        let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
        let origin_at_bottom = if terminal.mode().contains(TermMode::VI) {
            terminal.vi_mode_cursor.point.line == num_lines - 1
        } else {
            search_state.direction == Direction::Left
        };

        display.handle_update(
            terminal,
            notifier,
            message_buffer,
            search_state.history_index.is_some(),
            config,
        );

        let new_is_searching = search_state.history_index.is_some();
        if !old_is_searching && new_is_searching {
            // Scroll on search start to make sure origin is visible with minimal viewport motion.
            let display_offset = terminal.grid().display_offset();
            if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(1));
            } else if display_offset != 0 && origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(-1));
            }
        }
    }

    /// Return `true` if `event_queue` is empty, `false` otherwise.
    #[inline]
    #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
    fn event_queue_empty(&mut self) -> bool {
        let wayland_event_queue = match self.display.wayland_event_queue.as_mut() {
            Some(wayland_event_queue) => wayland_event_queue,
            // Since frame callbacks do not exist on X11, just check for event queue.
            None => return self.event_queue.is_empty(),
        };

        // Check for pending frame callbacks on Wayland.
        let events_dispatched = wayland_event_queue
            .dispatch_pending(&mut (), |_, _, _| {})
            .expect("failed to dispatch event queue");

        self.event_queue.is_empty() && events_dispatched == 0
    }

    /// Return `true` if `event_queue` is empty, `false` otherwise.
    #[inline]
    #[cfg(any(not(feature = "wayland"), target_os = "macos", windows))]
    fn event_queue_empty(&mut self) -> bool {
        self.event_queue.is_empty()
    }
}
