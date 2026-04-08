use crate::protocol::{ClientMsg, DaemonMsg, SessionInfo, recv_msg, send_msg};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph},
};
use std::{
    io::{self},
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
    thread,
};

const SIDEBAR_BG: Color = Color::Rgb(18, 18, 28);
const BAR_BG: Color = Color::Rgb(18, 18, 28);

/// Prefix key for mbulet commands (e.g., Ctrl+B). Change this single constant to use a different prefix.
const PREFIX_KEY: char = 'b';
const PREFIX_MODIFIERS: KeyModifiers = KeyModifiers::CONTROL;

struct ClientSession {
    id: usize,
    name: String,
    parser: Arc<Mutex<vt100::Parser>>,
    cwd: Option<String>,
}

#[derive(PartialEq)]
enum Focus {
    Sidebar,
    Terminal,
}

#[derive(PartialEq)]
enum PrefixMode {
    Normal,
    Pending, // waiting for the command key after prefix
}

struct App {
    sessions: Vec<ClientSession>,
    selected: usize,
    list_state: ListState,
    focus: Focus,
    rename_mode: bool,
    rename_input: String,
    worktree_mode: bool,
    worktree_input: String,
    term_cols: u16,
    term_rows: u16,
    attached_id: Option<usize>,
}

impl App {
    fn new(sessions: Vec<SessionInfo>, term_cols: u16, term_rows: u16) -> Self {
        let client_sessions: Vec<ClientSession> = sessions
            .into_iter()
            .map(|s| ClientSession {
                id: s.id,
                name: s.name,
                cwd: s.cwd,
                parser: Arc::new(Mutex::new(vt100::Parser::new(
                    term_rows.max(1),
                    term_cols.max(1),
                    0,
                ))),
            })
            .collect();

        let mut list_state = ListState::default();
        if !client_sessions.is_empty() {
            list_state.select(Some(0));
        }

        Self {
            sessions: client_sessions,
            selected: 0,
            list_state,
            focus: Focus::Sidebar,
            rename_mode: false,
            rename_input: String::new(),
            worktree_mode: false,
            worktree_input: String::new(),
            term_cols,
            term_rows,
            attached_id: None,
        }
    }

    fn current_id(&self) -> Option<usize> {
        self.sessions.get(self.selected).map(|s| s.id)
    }

    fn next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.sessions.len();
        self.list_state.select(Some(self.selected));
    }

    fn prev(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.sessions.len() - 1
        } else {
            self.selected - 1
        };
        self.list_state.select(Some(self.selected));
    }

    fn move_session_up(&mut self) -> Option<(usize, usize)> {
        if self.sessions.is_empty() || self.selected == 0 {
            return None;
        }
        let id = self.sessions[self.selected].id;
        let new_index = self.selected - 1;
        // Client-side optimistic update
        self.sessions.swap(self.selected, new_index);
        self.selected = new_index;
        self.list_state.select(Some(new_index));
        Some((id, new_index))
    }

    fn move_session_down(&mut self) -> Option<(usize, usize)> {
        if self.sessions.is_empty() || self.selected >= self.sessions.len() - 1 {
            return None;
        }
        let id = self.sessions[self.selected].id;
        let new_index = self.selected + 1;
        // Client-side optimistic update
        self.sessions.swap(self.selected, new_index);
        self.selected = new_index;
        self.list_state.select(Some(new_index));
        Some((id, new_index))
    }
}

/// Calculate the actual terminal pane size from the UI layout.
/// This must match the constraints in ui() to avoid parser/render desync.
fn pane_size(cols: u16, rows: u16) -> (u16, u16) {
    // Match the UI layout exactly:
    // - Vertical: 1 (top bar) + content + 1 (bottom bar)
    // - Horizontal: 22 (sidebar) + terminal
    // - Terminal has borders: -2 for left/right, -2 for top/bottom
    let content_rows = rows.saturating_sub(1 + 1); // top + bottom bars
    let term_rows = content_rows.saturating_sub(2).max(1); // border overhead
    
    let content_cols = cols.saturating_sub(22); // sidebar width
    let term_cols = content_cols.saturating_sub(2).max(1); // border overhead
    
    (term_cols, term_rows)
}

pub fn run_client(socket_path: &str) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path).map_err(|e| {
        io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("cannot connect to daemon: {}", e),
        )
    })?;

    // Get session list
    send_msg(&mut stream, &ClientMsg::ListSessions)?;
    let sessions = match recv_msg::<_, DaemonMsg>(&mut stream)? {
        DaemonMsg::SessionList { sessions } => sessions,
        DaemonMsg::Error { msg } => {
            return Err(io::Error::other(format!("daemon error: {}", msg)));
        }
        _ => return Err(io::Error::other("unexpected response")),
    };

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let size = terminal.size()?;
    let (term_cols, term_rows) = pane_size(size.width, size.height);

    let app = Arc::new(Mutex::new(App::new(sessions, term_cols, term_rows)));

    // Shared stream for writing
    let stream_write = Arc::new(Mutex::new(stream.try_clone()?));

    // Background thread: read daemon messages, update parsers
    {
        let app = Arc::clone(&app);
        let mut read_stream = stream.try_clone()?;
        let sw = Arc::clone(&stream_write);
        thread::spawn(move || {
            loop {
                // Use a short timeout so we don't block forever and starve the render loop
                read_stream
                    .set_read_timeout(Some(std::time::Duration::from_millis(20)))
                    .ok();
                match recv_msg::<_, DaemonMsg>(&mut read_stream) {
                    Ok(msg) => {
                        // For PtyOutput, grab the parser Arc first without holding app lock
                        // during the actual processing
                        match msg {
                            DaemonMsg::PtyOutput { id, data } => {
                                let parser = {
                                    let app = app.lock().unwrap();
                                    app.sessions
                                        .iter()
                                        .find(|s| s.id == id)
                                        .map(|s| Arc::clone(&s.parser))
                                };
                                if let Some(parser) = parser {
                                    parser.lock().unwrap().process(&data);
                                }
                            }
                            DaemonMsg::SessionCreated { id, name } => {
                                let (tc, tr) = {
                                    let mut app = app.lock().unwrap();
                                    let (tc, tr) = (app.term_cols, app.term_rows);
                                    app.sessions.push(ClientSession {
                                        id,
                                        name,
                                        cwd: None,
                                        parser: Arc::new(Mutex::new(vt100::Parser::new(
                                            tr.max(1),
                                            tc.max(1),
                                            0,
                                        ))),
                                    });
                                    let new_idx = app.sessions.len() - 1;
                                    app.selected = new_idx;
                                    app.list_state.select(Some(new_idx));
                                    (tc, tr)
                                };
                                // auto-attach to the new session (lock released above)
                                let _ = send_msg(
                                    &mut *sw.lock().unwrap(),
                                    &ClientMsg::Attach { id, cols: tc, rows: tr },
                                );
                            }
                            DaemonMsg::SessionDeleted { id } => {
                                let mut app = app.lock().unwrap();
                                app.sessions.retain(|s| s.id != id);
                                if app.sessions.is_empty() {
                                    app.selected = 0;
                                    app.list_state.select(None);
                                } else {
                                    if app.selected >= app.sessions.len() {
                                        app.selected = app.sessions.len() - 1;
                                    }
                                    let selected = app.selected;
                                    app.list_state.select(Some(selected));
                                }
                            }
                            DaemonMsg::SessionRenamed { id, name } => {
                                let mut app = app.lock().unwrap();
                                if let Some(s) = app.sessions.iter_mut().find(|s| s.id == id) {
                                    s.name = name;
                                }
                            }
                            DaemonMsg::SessionReordered { id, new_index } => {
                                let mut app = app.lock().unwrap();
                                if let Some(old_index) = app.sessions.iter().position(|s| s.id == id) {
                                    if new_index < app.sessions.len() {
                                        let session = app.sessions.remove(old_index);
                                        app.sessions.insert(new_index, session);
                                        // Keep selection stable: if this was the selected session, update selection
                                        if app.selected == old_index {
                                            app.selected = new_index;
                                            app.list_state.select(Some(new_index));
                                        } else {
                                            // Adjust selection index if another session was moved around it
                                            let selected = if old_index < app.selected && new_index >= app.selected {
                                                app.selected - 1
                                            } else if old_index > app.selected && new_index <= app.selected {
                                                app.selected + 1
                                            } else {
                                                app.selected
                                            };
                                            app.selected = selected;
                                            app.list_state.select(Some(selected));
                                        }
                                    }
                                }
                            }
                            DaemonMsg::CwdUpdate { id, cwd } => {
                                let mut app = app.lock().unwrap();
                                if let Some(s) = app.sessions.iter_mut().find(|s| s.id == id) {
                                    s.cwd = Some(cwd);
                                }
                            }
                            DaemonMsg::Attached { id, cleared: _ } => {
                                let mut app = app.lock().unwrap();
                                app.attached_id = Some(id);
                                // Parser was already reset when Attach was sent (before server
                                // replayed buffered output), so no action needed here.
                            }
                            DaemonMsg::Detached => {
                                app.lock().unwrap().attached_id = None;
                            }
                            DaemonMsg::Error { .. }
                            | DaemonMsg::SessionList { .. }
                            | DaemonMsg::Ok => {}
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        // timeout — just loop, gives render loop a chance to run
                        continue;
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Attach to first session with real terminal size
    {
        let (id, tc, tr) = {
            let a = app.lock().unwrap();
            (a.current_id(), a.term_cols, a.term_rows)
        };
        if let Some(id) = id {
            // Reset parser before sending initial attach
            {
                let app = app.lock().unwrap();
                if let Some(s) = app.sessions.iter().find(|s| s.id == id) {
                    *s.parser.lock().unwrap() = vt100::Parser::new(tr.max(1), tc.max(1), 0);
                }
            }
            send_msg(
                &mut *stream_write.lock().unwrap(),
                &ClientMsg::Attach { id, cols: tc, rows: tr },
            )?;
        }
    }

    let mut shutdown = false;
    let mut detach = false;
    let mut prefix_mode = PrefixMode::Normal;

    // Actions that require work outside the app lock (sending messages after lock release)
    enum Action {
        None,
        Detach,
        Shutdown,
        SendMsg(crate::protocol::ClientMsg),
    }

    loop {
        {
            let mut app = app.lock().unwrap();
            terminal.draw(|f| ui(f, &mut app, prefix_mode == PrefixMode::Pending))?;
        }

        if event::poll(std::time::Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // All app mutation happens in this block; lock is released at end of block
                    let action = {
                        let mut app = app.lock().unwrap();

                        // --- Prefix pending: next key is a mbulet command ---
                        if prefix_mode == PrefixMode::Pending {
                            prefix_mode = PrefixMode::Normal;
                            match (key.code, key.modifiers) {
                                (KeyCode::Char('d'), KeyModifiers::NONE) => Action::Detach,
                                (KeyCode::Char('q'), KeyModifiers::NONE) => Action::Shutdown,
                                (KeyCode::Tab, _) => {
                                    app.focus = Focus::Sidebar;
                                    Action::None
                                }
                                (KeyCode::Char('w'), KeyModifiers::NONE) => {
                                    if app.focus == Focus::Sidebar {
                                        app.worktree_mode = true;
                                        app.worktree_input.clear();
                                    }
                                    Action::None
                                }
                                (KeyCode::Up, _) => {
                                    if app.focus == Focus::Sidebar {
                                        // Sidebar focus: move selected session up
                                        if let Some((id, new_index)) = app.move_session_up() {
                                            Action::SendMsg(ClientMsg::ReorderSession { id, new_index })
                                        } else {
                                            Action::None
                                        }
                                    } else {
                                        // Terminal focus: switch to previous session
                                        if !app.sessions.is_empty() {
                                            app.prev();
                                            if let Some(id) = app.current_id() {
                                                let (tc, tr) = (app.term_cols, app.term_rows);
                                                Action::SendMsg(ClientMsg::Attach { id, cols: tc, rows: tr })
                                            } else {
                                                Action::None
                                            }
                                        } else {
                                            Action::None
                                        }
                                    }
                                }
                                (KeyCode::Down, _) => {
                                    if app.focus == Focus::Sidebar {
                                        // Sidebar focus: move selected session down
                                        if let Some((id, new_index)) = app.move_session_down() {
                                            Action::SendMsg(ClientMsg::ReorderSession { id, new_index })
                                        } else {
                                            Action::None
                                        }
                                    } else {
                                        // Terminal focus: switch to next session
                                        if !app.sessions.is_empty() {
                                            app.next();
                                            if let Some(id) = app.current_id() {
                                                let (tc, tr) = (app.term_cols, app.term_rows);
                                                Action::SendMsg(ClientMsg::Attach { id, cols: tc, rows: tr })
                                            } else {
                                                Action::None
                                            }
                                        } else {
                                            Action::None
                                        }
                                    }
                                }
                                // Prefix + Prefix → send literal prefix to terminal
                                (KeyCode::Char(c), m) if c == PREFIX_KEY && m == PREFIX_MODIFIERS => {
                                    if app.focus == Focus::Terminal {
                                        // Ctrl+B = 0x02
                                        let byte = if PREFIX_KEY == 'b' && PREFIX_MODIFIERS == KeyModifiers::CONTROL {
                                            0x02
                                        } else {
                                            // For other prefix keys, compute the control char
                                            (PREFIX_KEY as u8).to_ascii_lowercase().saturating_sub(b'a').saturating_add(1)
                                        };
                                        Action::SendMsg(ClientMsg::Input { data: vec![byte] })
                                    } else {
                                        Action::None
                                    }
                                }
                                _ => Action::None,
                            }

                        // --- Prefix key in any focus → enter pending mode ---
                        } else if key.code == KeyCode::Char(PREFIX_KEY) && key.modifiers == PREFIX_MODIFIERS {
                            prefix_mode = PrefixMode::Pending;
                            Action::None

                        } else if app.worktree_mode {
                            match key.code {
                                KeyCode::Enter => {
                                    let branch = app.worktree_input.trim().to_string();
                                    app.worktree_mode = false;
                                    app.worktree_input.clear();
                                    if !branch.is_empty() {
                                        let (tc, tr) = (app.term_cols, app.term_rows);
                                        let cwd = app.sessions.get(app.selected)
                                            .and_then(|s| s.cwd.clone());
                                        let cmd = if let Some(cwd) = cwd {
                                            format!(
                                                "cd {cwd:?} && git worktree add ../{0} -b {0} && cd ../{0}",
                                                branch
                                            )
                                        } else {
                                            format!(
                                                "git worktree add ../{0} -b {0} && cd ../{0}",
                                                branch
                                            )
                                        };
                                        Action::SendMsg(ClientMsg::NewSession {
                                            name: branch,
                                            cols: tc,
                                            rows: tr,
                                            startup_cmds: vec![cmd],
                                        })
                                    } else {
                                        Action::None
                                    }
                                }
                                KeyCode::Esc => {
                                    app.worktree_mode = false;
                                    app.worktree_input.clear();
                                    Action::None
                                }
                                KeyCode::Backspace => { app.worktree_input.pop(); Action::None }
                                KeyCode::Char(c) => { app.worktree_input.push(c); Action::None }
                                _ => Action::None,
                            }

                        } else if app.rename_mode {
                            match key.code {
                                KeyCode::Enter => {
                                    let name = app.rename_input.trim().to_string();
                                    let id = app.current_id();
                                    app.rename_mode = false;
                                    app.rename_input.clear();
                                    if !name.is_empty() {
                                        if let Some(id) = id {
                                            let selected = app.selected;
                                            if let Some(s) = app.sessions.get_mut(selected) {
                                                s.name = name.clone();
                                            }
                                            Action::SendMsg(ClientMsg::RenameSession { id, name })
                                        } else {
                                            Action::None
                                        }
                                    } else {
                                        Action::None
                                    }
                                }
                                KeyCode::Esc => {
                                    app.rename_mode = false;
                                    app.rename_input.clear();
                                    Action::None
                                }
                                KeyCode::Backspace => { app.rename_input.pop(); Action::None }
                                KeyCode::Char(c) => { app.rename_input.push(c); Action::None }
                                _ => Action::None,
                            }

                        } else {
                            match app.focus {
                                Focus::Sidebar => match (key.code, key.modifiers) {
                                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                                        app.next();
                                        if let Some(id) = app.current_id() {
                                            let (tc, tr) = (app.term_cols, app.term_rows);
                                            Action::SendMsg(ClientMsg::Attach { id, cols: tc, rows: tr })
                                        } else { Action::None }
                                    }
                                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                                        app.prev();
                                        if let Some(id) = app.current_id() {
                                            let (tc, tr) = (app.term_cols, app.term_rows);
                                            Action::SendMsg(ClientMsg::Attach { id, cols: tc, rows: tr })
                                        } else { Action::None }
                                    }
                                    (KeyCode::Char('n'), _) => {
                                        let count = app.sessions.len() + 1;
                                        let name = format!("session-{}", count);
                                        let (tc, tr) = (app.term_cols, app.term_rows);
                                        Action::SendMsg(ClientMsg::NewSession { name, cols: tc, rows: tr, startup_cmds: vec![] })
                                    }
                                    (KeyCode::Char('r'), _) => {
                                        app.rename_input = app.sessions.get(app.selected)
                                            .map(|s| s.name.clone()).unwrap_or_default();
                                        app.rename_mode = true;
                                        Action::None
                                    }
                                    (KeyCode::Char('d'), _) => {
                                        if let Some(id) = app.current_id() {
                                            Action::SendMsg(ClientMsg::DeleteSession { id })
                                        } else { Action::None }
                                    }
                                    (KeyCode::Enter, _) | (KeyCode::Tab, _) => {
                                        app.focus = Focus::Terminal;
                                        Action::None
                                    }
                                    _ => Action::None,
                                },
                                Focus::Terminal => {
                                    if let Some(bytes) = key_to_bytes(key) {
                                        Action::SendMsg(ClientMsg::Input { data: bytes })
                                    } else {
                                        Action::None
                                    }
                                }
                            }
                        }
                        // lock released here — app guard drops
                    };

                    // Act on the result outside the lock
                    match action {
                        Action::Detach => { detach = true; break; }
                        Action::Shutdown => { shutdown = true; break; }
                        Action::SendMsg(msg) => {
                            // Reset parser BEFORE sending Attach to ensure clean slate
                            // when server replays buffered output
                            if let ClientMsg::Attach { id, cols, rows } = &msg {
                                let app = app.lock().unwrap();
                                if let Some(s) = app.sessions.iter().find(|s| s.id == *id) {
                                    *s.parser.lock().unwrap() = vt100::Parser::new(*rows.max(&1), *cols.max(&1), 0);
                                }
                            }
                            let _ = send_msg(&mut *stream_write.lock().unwrap(), &msg);
                        }
                        Action::None => {}
                    }
                }

                Event::Resize(cols, rows) => {
                    let (tc, tr) = pane_size(cols, rows);
                    let action = {
                        let mut app = app.lock().unwrap();
                        app.term_cols = tc;
                        app.term_rows = tr;
                        if app.attached_id.is_some() {
                            Some(ClientMsg::Resize { cols: tc, rows: tr })
                        } else {
                            None
                        }
                    };
                    if let Some(msg) = action {
                        let _ = send_msg(&mut *stream_write.lock().unwrap(), &msg);
                    }
                }

                _ => {}
            }
        }
    }

    // Cleanup TUI
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if shutdown {
        let _ = send_msg(&mut *stream_write.lock().unwrap(), &ClientMsg::Shutdown);
        println!("mbulet: daemon shut down.");
    } else if detach {
        let _ = send_msg(&mut *stream_write.lock().unwrap(), &ClientMsg::Detach);
        println!("mbulet: detached. Sessions running in background. Run 'mbulet' to reattach.");
    }

    Ok(())
}

fn ui(frame: &mut ratatui::Frame, app: &mut App, ctrl_m_pending: bool) {
    let full = frame.area();
    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(full);

    draw_topbar(frame, app, vchunks[0]);

    let hchunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Min(0)])
        .split(vchunks[1]);

    draw_sidebar(frame, app, hchunks[0]);
    draw_terminal(frame, app, hchunks[1]);
    draw_footer(frame, app, vchunks[2], ctrl_m_pending);
}

fn draw_topbar(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let n = app.sessions.len();
    let right_text = format!("Sessions: {}  ", n);
    let bar = Paragraph::new(Line::from(Span::styled(
        "  mbulet",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
            .bg(BAR_BG),
    )))
    .style(Style::default().bg(BAR_BG))
    .alignment(Alignment::Left);
    frame.render_widget(bar, area);

    let right_width = right_text.len() as u16;
    if area.width > right_width {
        let right_area = Rect {
            x: area.x + area.width - right_width,
            y: area.y,
            width: right_width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                right_text,
                Style::default().fg(Color::DarkGray).bg(BAR_BG),
            )))
            .style(Style::default().bg(BAR_BG)),
            right_area,
        );
    }
}

fn draw_footer(frame: &mut ratatui::Frame, app: &App, area: Rect, ctrl_m_pending: bool) {
    // Helper closures for styled spans
    let key = |s: &str| Span::styled(s.to_string(), Style::default().fg(Color::Cyan).bg(BAR_BG));
    let sep = |s: &str| Span::styled(s.to_string(), Style::default().fg(Color::DarkGray).bg(BAR_BG));
    let warn = |s: &str| Span::styled(s.to_string(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD).bg(BAR_BG));

    // Display prefix key dynamically
    let prefix_display = if PREFIX_MODIFIERS == KeyModifiers::CONTROL {
        format!("^{}", PREFIX_KEY.to_uppercase())
    } else if PREFIX_MODIFIERS == KeyModifiers::ALT {
        format!("M-{}", PREFIX_KEY)
    } else {
        PREFIX_KEY.to_string()
    };

    // Prefix pending: show command mode indicator regardless of focus
    let text = if ctrl_m_pending {
        Line::from(vec![
            warn(&format!("  ⌨  {} ", prefix_display)),
            sep("→ "),
            key("d"), sep(": detach   "),
            key("q"), sep(": shutdown   "),
            key("Tab"), sep(": sidebar   "),
            key(&prefix_display), sep(": send prefix   "),
            key("w"), sep(": worktree   "),
            key("↑/↓"), sep(": move session  "),
        ])
    } else if app.worktree_mode {
        Line::from(vec![
            sep("  "),
            warn("worktree branch: "),
            sep("type branch name  "),
            key("Enter"), sep(": create   "),
            key("Esc"), sep(": cancel  "),
        ])
    } else if app.rename_mode {
        Line::from(vec![
            sep("  "),
            key("Esc"), sep(": cancel   "),
            key("Enter"), sep(": confirm  "),
        ])
    } else if app.focus == Focus::Sidebar {
        Line::from(vec![
            sep("  "),
            key("j/k"), sep(": nav   "),
            key("n"), sep(": new   "),
            key("r"), sep(": rename   "),
            key("d"), sep(": delete   "),
            key("Enter"), sep(": open   "),
            key(&format!("{} w", prefix_display)), sep(": worktree   "),
            key(&format!("{} d", prefix_display)), sep(": detach   "),
            key(&format!("{} q", prefix_display)), sep(": shutdown  "),
        ])
    } else {
        // Terminal focus
        Line::from(vec![
            sep("  "),
            key(&format!("{} ↑/↓", prefix_display)), sep(": switch session   "),
            key(&format!("{} Tab", prefix_display)), sep(": sidebar   "),
            key(&format!("{} d", prefix_display)), sep(": detach   "),
            key(&format!("{} q", prefix_display)), sep(": shutdown   "),
            key(&format!("{} {}", prefix_display, prefix_display)), sep(": send prefix  "),
        ])
    };
    frame.render_widget(Paragraph::new(text).style(Style::default().bg(BAR_BG)), area);
}

fn draw_sidebar(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Sidebar && !app.rename_mode && !app.worktree_mode;

    let input_active = app.rename_mode || app.worktree_mode;
    let (list_area, input_area) = if input_active && area.height > 5 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            ListItem::new(Line::from(vec![
                Span::styled(" ● ", Style::default().fg(Color::Green).bg(SIDEBAR_BG)),
                Span::styled(
                    s.name.clone(),
                    Style::default().fg(Color::White).bg(SIDEBAR_BG),
                ),
            ]))
        })
        .collect();

    let border_color = if focused { Color::Cyan } else { Color::DarkGray };

    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    " sessions ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                        .bg(SIDEBAR_BG),
                ))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(SIDEBAR_BG)),
        )
        .style(Style::default().bg(SIDEBAR_BG))
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 60, 120))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, list_area, &mut app.list_state);

    if let Some(ia) = input_area {
        if app.rename_mode {
            let widget = Paragraph::new(Line::from(vec![
                Span::styled("Name: ", Style::default().fg(Color::Cyan).bg(SIDEBAR_BG)),
                Span::styled(
                    format!("{}_", app.rename_input),
                    Style::default().fg(Color::White).bg(SIDEBAR_BG),
                ),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan))
                    .style(Style::default().bg(SIDEBAR_BG)),
            )
            .style(Style::default().bg(SIDEBAR_BG));
            frame.render_widget(widget, ia);
        } else if app.worktree_mode {
            let widget = Paragraph::new(Line::from(vec![
                Span::styled("Branch: ", Style::default().fg(Color::Yellow).bg(SIDEBAR_BG)),
                Span::styled(
                    format!("{}_", app.worktree_input),
                    Style::default().fg(Color::White).bg(SIDEBAR_BG),
                ),
            ]))
            .block(
                Block::default()
                    .title(Span::styled(
                        " worktree ",
                        Style::default().fg(Color::Yellow).bg(SIDEBAR_BG),
                    ))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Yellow))
                    .style(Style::default().bg(SIDEBAR_BG)),
            )
            .style(Style::default().bg(SIDEBAR_BG));
            frame.render_widget(widget, ia);
        }
    }
}

fn draw_terminal(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Terminal;
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };

    let session = match app.sessions.get(app.selected) {
        Some(s) => s,
        None => return,
    };

    let title = format!(" {} ", session.name);
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    frame.render_widget(
        Block::default()
            .title(Span::styled(
                title,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color)),
        area,
    );

    // Ensure parser dimensions match the actual render area before drawing.
    // This prevents desync between calculated size and actual allocated space.
    {
        let mut parser = session.parser.lock().unwrap();
        let (parser_rows, parser_cols) = parser.screen().size();
        if parser_rows != inner.height || parser_cols != inner.width {
            parser.set_size(inner.height, inner.width);
        }
    }

    let (lines, cursor_pos) = {
        let screen = session.parser.lock().unwrap();
        let vt = screen.screen();
        let lines: Vec<Line> = (0..inner.height)
            .map(|row| {
                Line::from(
                    (0..inner.width)
                        .map(|col| {
                            if let Some(cell) = vt.cell(row, col) {
                                let contents = cell.contents();
                                // Skip wide-char continuation cells (empty string after a wide char)
                                // and any non-printable content — render as space
                                let ch = if contents.is_empty() {
                                    " ".to_string()
                                } else {
                                    // Filter out any content containing control characters
                                    // (catches single \x1b chars AND multi-byte strings like "3\x1b")
                                    if contents.chars().any(|c| c.is_control()) {
                                        " ".to_string()
                                    } else {
                                        // DEBUG: log suspicious-looking short strings that look like escape params
                                        #[cfg(debug_assertions)]
                                        if contents.len() <= 4 && contents.chars().all(|c| c.is_ascii_digit() || c == ':' || c == ';') {
                                            let bytes: Vec<u8> = contents.bytes().collect();
                                            let _ = std::fs::OpenOptions::new()
                                                .create(true).append(true)
                                                .open("/tmp/mbulet-cells.log")
                                                .map(|mut f| {
                                                    use std::io::Write;
                                                    let _ = writeln!(f, "row={row} col={col} contents={contents:?} bytes={bytes:?} fgcolor={:?} bgcolor={:?}", cell.fgcolor(), cell.bgcolor());
                                                });
                                        }
                                        contents
                                    }
                                };
                                let mut style = Style::default()
                                    .fg(vt100_color(cell.fgcolor()))
                                    .bg(vt100_color(cell.bgcolor()));
                                if cell.bold() {
                                    style = style.add_modifier(Modifier::BOLD);
                                }
                                if cell.italic() {
                                    style = style.add_modifier(Modifier::ITALIC);
                                }
                                if cell.underline() {
                                    style = style.add_modifier(Modifier::UNDERLINED);
                                }
                                Span::styled(ch, style)
                            } else {
                                Span::raw(" ")
                            }
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        (lines, vt.cursor_position())
    };

    frame.render_widget(Paragraph::new(lines), inner);

    if focused {
        let (crow, ccol) = cursor_pos;
        if crow < inner.height && ccol < inner.width {
            frame.set_cursor_position((inner.x + ccol, inner.y + crow));
        }
    }
}

fn vt100_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(0) => Color::Black,
        vt100::Color::Idx(1) => Color::Red,
        vt100::Color::Idx(2) => Color::Green,
        vt100::Color::Idx(3) => Color::Yellow,
        vt100::Color::Idx(4) => Color::Blue,
        vt100::Color::Idx(5) => Color::Magenta,
        vt100::Color::Idx(6) => Color::Cyan,
        vt100::Color::Idx(7) => Color::Gray,
        vt100::Color::Idx(8) => Color::DarkGray,
        vt100::Color::Idx(9) => Color::LightRed,
        vt100::Color::Idx(10) => Color::LightGreen,
        vt100::Color::Idx(11) => Color::LightYellow,
        vt100::Color::Idx(12) => Color::LightBlue,
        vt100::Color::Idx(13) => Color::LightMagenta,
        vt100::Color::Idx(14) => Color::LightCyan,
        vt100::Color::Idx(15) => Color::White,
        vt100::Color::Idx(n) => Color::Indexed(n),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn key_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use KeyCode::*;
    Some(match key.code {
        Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let b = (c as u8).to_ascii_lowercase();
                if b.is_ascii_lowercase() {
                    vec![b - b'a' + 1]
                } else {
                    return None;
                }
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf);
                buf[..c.len_utf8()].to_vec()
            }
        }
        Enter => vec![b'\r'],
        Backspace => vec![0x7f],
        Esc => vec![0x1b],
        Tab => vec![b'\t'],  // pass through to shell (vim tab, autocomplete, etc.)
        Up => b"\x1b[A".to_vec(),
        Down => b"\x1b[B".to_vec(),
        Right => b"\x1b[C".to_vec(),
        Left => b"\x1b[D".to_vec(),
        Home => b"\x1b[H".to_vec(),
        End => b"\x1b[F".to_vec(),
        Delete => b"\x1b[3~".to_vec(),
        PageUp => b"\x1b[5~".to_vec(),
        PageDown => b"\x1b[6~".to_vec(),
        F(1) => b"\x1bOP".to_vec(),
        F(2) => b"\x1bOQ".to_vec(),
        F(3) => b"\x1bOR".to_vec(),
        F(4) => b"\x1bOS".to_vec(),
        F(n) => format!("\x1b[{}~", n + 10).into_bytes(),
        _ => return None,
    })
}
