// betterm — a small GPU terminal for Windows.
//
// Borderless winit window -> ConPTY hosting the shell -> vte parser building a
// cell grid -> wgpu renderer (background, glyphs, animated RGB border).

#![cfg_attr(windows, windows_subsystem = "windows")] // no extra console window

mod banner;
mod config;
mod font;
mod logx;
mod paths;
mod pty;
mod renderer;
mod terminal;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, ResizeDirection, Window, WindowId};

use config::Config;
use font::FontAtlas;
use pty::Pty;
use renderer::Renderer;
use terminal::Term;

#[derive(Debug)]
enum UserEvent {
    /// New PTY output is available.
    Wakeup,
    /// The shell exited.
    Exit,
}

struct App {
    cfg: Config,
    proxy: EventLoopProxy<UserEvent>,

    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    atlas: Option<FontAtlas>,
    pty: Option<Pty>,
    term: Arc<Mutex<Term>>,
    clipboard: Option<arboard::Clipboard>,

    mods: ModifiersState,
    start: Instant,
    mouse_pos: PhysicalPosition<f64>,
    selecting: bool,
    sel_anchor: Option<(usize, usize)>,
    sel_cursor: Option<(usize, usize)>,
    /// Last applied resize-edge cursor, to avoid redundant updates.
    cursor_dir: Option<ResizeDirection>,
}

impl App {
    fn new(cfg: Config, proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            cfg,
            proxy,
            window: None,
            renderer: None,
            atlas: None,
            pty: None,
            term: Arc::new(Mutex::new(Term::new(80, 24))),
            clipboard: arboard::Clipboard::new().ok(),
            mods: ModifiersState::empty(),
            start: Instant::now(),
            mouse_pos: PhysicalPosition::new(0.0, 0.0),
            selecting: false,
            sel_anchor: None,
            sel_cursor: None,
            cursor_dir: None,
        }
    }

    /// Which window edge/corner is under the pointer (for borderless resize).
    fn resize_dir(&self, pos: PhysicalPosition<f64>) -> Option<ResizeDirection> {
        let r = self.renderer.as_ref()?;
        // Grab thickness in physical px, scaled for HiDPI so it's easy to hit.
        let scale = self
            .window
            .as_ref()
            .map(|w| w.scale_factor() as f32)
            .unwrap_or(1.0);
        let grab = 9.0 * scale;
        let (w, h) = (r.size.width as f32, r.size.height as f32);
        let (x, y) = (pos.x as f32, pos.y as f32);
        let left = x <= grab;
        let right = x >= w - grab;
        let top = y <= grab;
        let bottom = y >= h - grab;
        match (top, bottom, left, right) {
            (true, _, true, _) => Some(ResizeDirection::NorthWest),
            (true, _, _, true) => Some(ResizeDirection::NorthEast),
            (_, true, true, _) => Some(ResizeDirection::SouthWest),
            (_, true, _, true) => Some(ResizeDirection::SouthEast),
            (true, ..) => Some(ResizeDirection::North),
            (_, true, ..) => Some(ResizeDirection::South),
            (_, _, true, _) => Some(ResizeDirection::West),
            (_, _, _, true) => Some(ResizeDirection::East),
            _ => None,
        }
    }

    /// Convert a physical mouse position into a (col, row) grid cell.
    fn cell_at(&self, pos: PhysicalPosition<f64>) -> (usize, usize) {
        let (Some(r), Some(a)) = (&self.renderer, &self.atlas) else {
            return (0, 0);
        };
        let term = self.term.lock().unwrap();
        let col = (((pos.x as f32 - r.pad) / a.cell_w).floor().max(0.0) as usize)
            .min(term.cols.saturating_sub(1));
        let row = (((pos.y as f32 - r.title_h) / a.cell_h).floor().max(0.0) as usize)
            .min(term.rows.saturating_sub(1));
        (col, row)
    }

    /// Which window-control button (if any) is under a physical position.
    fn button_at(&self, pos: PhysicalPosition<f64>) -> Option<renderer::Control> {
        let r = self.renderer.as_ref()?;
        r.buttons().into_iter().find_map(|b| {
            let (dx, dy) = (pos.x as f32 - b.cx, pos.y as f32 - b.cy);
            ((dx * dx + dy * dy).sqrt() <= b.r + 3.0).then_some(b.control)
        })
    }

    /// Current selection as an inclusive linear cell range (normalized).
    fn selection_range(&self) -> Option<(usize, usize)> {
        let (a, c) = (self.sel_anchor?, self.sel_cursor?);
        if a == c {
            return None;
        }
        let cols = self.term.lock().unwrap().cols;
        let ai = a.1 * cols + a.0;
        let ci = c.1 * cols + c.0;
        Some((ai.min(ci), ai.max(ci)))
    }

    fn copy_selection(&mut self) {
        let Some((s, e)) = self.selection_range() else {
            return;
        };
        let text = {
            let term = self.term.lock().unwrap();
            let cols = term.cols;
            let (sr, er) = (s / cols, e / cols);
            let mut out = String::new();
            for r in sr..=er {
                let cstart = if r == sr { s % cols } else { 0 };
                let cend = if r == er { e % cols } else { cols - 1 };
                let mut line = String::new();
                for c in cstart..=cend {
                    line.push(term.cells[r * cols + c].ch);
                }
                while line.ends_with(' ') {
                    line.pop();
                }
                out.push_str(&line);
                if r != er {
                    out.push_str("\r\n");
                }
            }
            out
        };
        if let Some(cb) = &mut self.clipboard {
            let _ = cb.set_text(text);
        }
    }

    fn paste(&mut self) {
        let text = self
            .clipboard
            .as_mut()
            .and_then(|cb| cb.get_text().ok())
            .unwrap_or_default();
        if text.is_empty() {
            return;
        }
        // Shells expect CR for Enter.
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        if let Some(pty) = &mut self.pty {
            pty.write(normalized.as_bytes());
        }
    }

    fn handle_key(&mut self, ev: KeyEvent) {
        if ev.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.mods.control_key();
        let alt = self.mods.alt_key();

        if ctrl {
            if let Key::Character(s) = &ev.logical_key {
                match s.chars().next().map(|c| c.to_ascii_lowercase()) {
                    Some('c') => {
                        if self.selection_range().is_some() {
                            self.copy_selection();
                        } else if let Some(pty) = &mut self.pty {
                            pty.write(b"\x03"); // SIGINT
                        }
                        return;
                    }
                    Some('v') => {
                        self.paste();
                        return;
                    }
                    _ => {}
                }
            }
        }

        let bytes: Option<Vec<u8>> = match &ev.logical_key {
            Key::Named(named) => {
                let seq: &[u8] = match named {
                    NamedKey::Enter => b"\r",
                    NamedKey::Backspace => b"\x7f",
                    NamedKey::Tab => b"\t",
                    NamedKey::Escape => b"\x1b",
                    NamedKey::ArrowUp => b"\x1b[A",
                    NamedKey::ArrowDown => b"\x1b[B",
                    NamedKey::ArrowRight => b"\x1b[C",
                    NamedKey::ArrowLeft => b"\x1b[D",
                    NamedKey::Home => b"\x1b[H",
                    NamedKey::End => b"\x1b[F",
                    NamedKey::Delete => b"\x1b[3~",
                    NamedKey::Insert => b"\x1b[2~",
                    NamedKey::PageUp => b"\x1b[5~",
                    NamedKey::PageDown => b"\x1b[6~",
                    NamedKey::Space => b" ",
                    _ => b"",
                };
                if seq.is_empty() {
                    ev.text.as_ref().map(|t| t.as_bytes().to_vec())
                } else {
                    Some(seq.to_vec())
                }
            }
            Key::Character(s) if ctrl && !alt => {
                // Ctrl+letter -> control code.
                let c = s.chars().next().unwrap_or('\0').to_ascii_lowercase();
                if c.is_ascii_alphabetic() {
                    Some(vec![c as u8 - b'a' + 1])
                } else {
                    ev.text.as_ref().map(|t| t.as_bytes().to_vec())
                }
            }
            _ => ev.text.as_ref().map(|t| t.as_bytes().to_vec()),
        };

        if let (Some(bytes), Some(pty)) = (bytes, &mut self.pty) {
            if !bytes.is_empty() {
                pty.write(&bytes);
            }
        }
    }

    fn recompute_grid(&mut self) {
        let (Some(r), Some(a)) = (&self.renderer, &self.atlas) else {
            return;
        };
        let (cols, rows) = r.grid_size(a);
        self.term
            .lock()
            .unwrap()
            .resize(cols as usize, rows as usize);
        if let Some(pty) = &mut self.pty {
            pty.resize(cols, rows);
        }
    }

    /// Reconcile the swapchain with the live window size and draw one frame.
    /// Also called from the `Resized` handler, whose drag-resize modal loop
    /// starves `RedrawRequested`.
    fn redraw(&mut self) {
        if let Some(w) = &self.window {
            let cur = w.inner_size();
            let stale = self.renderer.as_ref().is_some_and(|r| r.size != cur);
            if stale && cur.width > 0 && cur.height > 0 {
                if let Some(r) = &mut self.renderer {
                    r.resize(cur);
                }
                self.recompute_grid();
            }
        }

        let sel = self.selection_range();
        if let (Some(r), Some(a)) = (&mut self.renderer, &mut self.atlas) {
            let term = self.term.lock().unwrap();
            let t = self.start.elapsed().as_secs_f32();
            r.render(&term, a, t, sel);
        }
    }
}

impl App {
    /// Create the window, GPU renderer and shell.
    fn setup(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<()> {
        let attrs = Window::default_attributes()
            .with_title("betterm")
            .with_window_icon(load_window_icon()) // taskbar / Alt-Tab icon
            .with_decorations(false) // borderless: we draw our own RGB border
            .with_transparent(true)
            .with_resizable(true)
            .with_inner_size(LogicalSize::new(1000.0, 640.0));
        let window = Arc::new(event_loop.create_window(attrs)?);

        #[cfg(windows)]
        if self.cfg.acrylic {
            let c = self.cfg.background_color;
            let tint = (
                (c[0] * 255.0) as u8,
                (c[1] * 255.0) as u8,
                (c[2] * 255.0) as u8,
                (self.cfg.background_opacity * 255.0) as u8,
            );
            if let Err(e) = window_vibrancy::apply_acrylic(window.as_ref(), Some(tint)) {
                crate::logx::log(&format!("apply_acrylic failed: {e}"));
            }
        }

        let scale = window.scale_factor() as f32;
        let font_path = self
            .cfg
            .resolve_font()
            .ok_or_else(|| anyhow::anyhow!("no usable monospace font found"))?;
        let font_bytes = std::fs::read(&font_path)?;
        let atlas = FontAtlas::new(font_bytes, self.cfg.font_size * scale, self.cfg.line_height)?;
        let renderer = Renderer::new(window.clone(), &self.cfg, &atlas)?;

        let (cols, rows) = renderer.grid_size(&atlas);
        *self.term.lock().unwrap() = Term::new(cols as usize, rows as usize);

        // betterm generates the banner and writes it to a file the shell prints at
        // startup, so it lands above the prompt and scrolls away naturally (ConPTY
        // repaints the grid, so betterm can't inject it directly).
        let banner_path = self
            .cfg
            .banner
            .then(|| write_banner_file(&banner::build(&self.cfg.shell, &renderer.gpu_name)))
            .flatten();

        let args = prepare_shell_args(&self.cfg);
        let (pty, reader) = Pty::spawn(&self.cfg.shell, &args, cols, rows, banner_path.as_deref())?;
        self.pty = Some(pty);
        self.spawn_reader(reader);

        self.atlas = Some(atlas);
        self.renderer = Some(renderer);
        self.window = Some(window);
        event_loop.set_control_flow(ControlFlow::Poll);
        Ok(())
    }

    /// Parse PTY output into the shared grid on a background thread.
    fn spawn_reader(&self, mut reader: Box<dyn std::io::Read + Send>) {
        let term = self.term.clone();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let mut parser = vte::Parser::new();
            let mut buf = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = proxy.send_event(UserEvent::Exit);
                        break;
                    }
                    Ok(n) => {
                        if let Ok(mut t) = term.lock() {
                            for &b in &buf[..n] {
                                parser.advance(&mut *t, b);
                            }
                        }
                        let _ = proxy.send_event(UserEvent::Wakeup);
                    }
                }
            }
        });
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return; // already initialized
        }
        if let Err(e) = self.setup(event_loop) {
            eprintln!("[betterm] startup failed: {e}");
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size);
                }
                self.recompute_grid();
                self.redraw(); // draw now: drag-resize blocks RedrawRequested
            }

            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),

            WindowEvent::KeyboardInput { event, .. } => self.handle_key(event),

            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = position;
                if self.selecting {
                    self.sel_cursor = Some(self.cell_at(position));
                } else {
                    let dir = self.resize_dir(position);
                    if dir != self.cursor_dir {
                        self.cursor_dir = dir;
                        if let Some(w) = &self.window {
                            w.set_cursor(resize_cursor(dir));
                        }
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    match state {
                        ElementState::Pressed => {
                            let pos = self.mouse_pos;
                            // 0. Window edge -> borderless resize.
                            if let Some(dir) = self.resize_dir(pos) {
                                if let Some(w) = &self.window {
                                    let _ = w.drag_resize_window(dir);
                                }
                                return;
                            }
                            // 1. Window control buttons.
                            if let Some(ctrl) = self.button_at(pos) {
                                match ctrl {
                                    renderer::Control::Close => event_loop.exit(),
                                    renderer::Control::Minimize => {
                                        if let Some(w) = &self.window {
                                            w.set_minimized(true);
                                        }
                                    }
                                    renderer::Control::Maximize => {
                                        if let Some(w) = &self.window {
                                            w.set_maximized(!w.is_maximized());
                                        }
                                    }
                                }
                                return;
                            }
                            // 2. Title bar -> drag the window.
                            let title_h = self.renderer.as_ref().map(|r| r.title_h).unwrap_or(0.0);
                            if (pos.y as f32) < title_h {
                                if let Some(w) = &self.window {
                                    let _ = w.drag_window();
                                }
                                return;
                            }
                            // 3. Otherwise start a text selection.
                            let cell = self.cell_at(pos);
                            self.sel_anchor = Some(cell);
                            self.sel_cursor = Some(cell);
                            self.selecting = true;
                        }
                        ElementState::Released => {
                            self.selecting = false;
                            // A plain click (no drag) clears the selection.
                            if self.sel_anchor == self.sel_cursor {
                                self.sel_anchor = None;
                                self.sel_cursor = None;
                            }
                        }
                    }
                }
            }

            WindowEvent::RedrawRequested => self.redraw(),

            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wakeup => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Exit => event_loop.exit(),
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Keep redrawing so the RGB border animates.
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// Decode the embedded PNG logo into a winit window icon (taskbar + Alt-Tab).
/// The Explorer exe icon is a separate Windows resource (see `build.rs`).
fn load_window_icon() -> Option<winit::window::Icon> {
    const LOGO: &[u8] = include_bytes!("../assets/images/betterm.png");
    let img = image::load_from_memory(LOGO)
        .map_err(|e| logx::log(&format!("window icon decode failed: {e}")))
        .ok()?
        .resize(64, 64, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    let (w, h) = img.dimensions();
    winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

fn resize_cursor(dir: Option<ResizeDirection>) -> CursorIcon {
    match dir {
        Some(ResizeDirection::West | ResizeDirection::East) => CursorIcon::EwResize,
        Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
        Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => CursorIcon::NeswResize,
        Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => CursorIcon::NwseResize,
        None => CursorIcon::Default,
    }
}

/// Build the shell arguments. With `auto_profile`, inject betterm's init for
/// known shells (dot-sourced profile for PowerShell, `--rcfile` for Git Bash)
/// without editing the user's global profile. Explicit `shell_args` override it.
fn prepare_shell_args(cfg: &Config) -> Vec<String> {
    if !cfg.shell_args.is_empty() {
        return cfg.shell_args.clone();
    }
    if !cfg.auto_profile {
        return Vec::new();
    }
    let lower = cfg.shell.to_lowercase();
    if lower.contains("powershell") || lower.contains("pwsh") {
        if let Some(path) = paths::write_data_file(
            "profile.ps1",
            include_str!("../assets/betterm.profile.ps1"),
        ) {
            let escaped = path.display().to_string().replace('\'', "''");
            return vec![
                "-NoLogo".into(),
                "-NoExit".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-Command".into(),
                format!(". '{escaped}'"),
            ];
        }
    } else if lower.contains("bash") {
        if let Some(path) =
            paths::write_data_file("betterm.bashrc", include_str!("../assets/betterm.bashrc"))
        {
            // Interactive shell that sources our rc (which sources the user's
            // profile + bashrc, then prints the banner).
            return vec![
                "--rcfile".into(),
                path.display().to_string().replace('\\', "/"),
                "-i".into(),
            ];
        }
    }
    Vec::new()
}

/// Write the generated banner to a file and return its path with forward slashes
/// (so both PowerShell and Git Bash can read it).
fn write_banner_file(contents: &str) -> Option<String> {
    let path = paths::write_data_file("banner.ans", contents)?;
    Some(path.display().to_string().replace('\\', "/"))
}

fn main() -> anyhow::Result<()> {
    let cfg = Config::load();
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(cfg, proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}
