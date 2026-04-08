use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    thread,
};

pub struct Session {
    pub id: usize,
    pub name: String,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub writer: Box<dyn Write + Send>,
    pub cwd: Arc<Mutex<Option<String>>>,
    child: Box<dyn portable_pty::Child + Send>,
    _slave: Box<dyn portable_pty::SlavePty + Send>,
    master: Box<dyn MasterPty + Send>,
    /// Temp ZDOTDIR dir to clean up on drop
    zdotdir: std::path::PathBuf,
    /// Raw PTY output buffer (ring buffer of last 65536 bytes)
    pub output_buf: Arc<Mutex<Vec<u8>>>,
    /// Subscribers: channels to notify of new PTY output
    pub subscribers: Arc<Mutex<Vec<std::sync::mpsc::Sender<Vec<u8>>>>>,
    /// CWD subscribers: notified with new path when OSC 7 is parsed
    pub cwd_subscribers: Arc<Mutex<Vec<std::sync::mpsc::Sender<String>>>>,
}

/// Parse OSC 7 sequences out of raw PTY bytes.
/// OSC 7 format: `\x1b]7;file://hostname/path\x07` or `\x1b]7;file://hostname/path\x1b\\`
/// Returns all CWD strings found, and the input with OSC 7 sequences stripped.
fn extract_osc7(data: &[u8]) -> (Vec<String>, Vec<u8>) {
    let mut cwds = Vec::new();
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        // Look for ESC ] 7 ;
        if i + 3 < data.len()
            && data[i] == b'\x1b'
            && data[i + 1] == b']'
            && data[i + 2] == b'7'
            && data[i + 3] == b';'
        {
            // Scan for terminator: BEL (0x07) or ST (ESC \)
            let start = i + 4;
            let mut end = None;
            let mut j = start;
            while j < data.len() {
                if data[j] == b'\x07' {
                    end = Some((j, j + 1));
                    break;
                }
                if j + 1 < data.len() && data[j] == b'\x1b' && data[j + 1] == b'\\' {
                    end = Some((j, j + 2));
                    break;
                }
                j += 1;
            }
            if let Some((payload_end, skip_to)) = end {
                let payload = &data[start..payload_end];
                if let Ok(s) = std::str::from_utf8(payload) {
                    // Strip file://hostname prefix → keep only path
                    let path = if let Some(rest) = s.strip_prefix("file://") {
                        // rest = "hostname/path" — drop up to first /
                        if let Some(slash) = rest.find('/') {
                            &rest[slash..]
                        } else {
                            rest
                        }
                    } else {
                        s
                    };
                    if !path.is_empty() {
                        cwds.push(path.to_string());
                    }
                }
                i = skip_to;
                // Don't append OSC 7 to output — strip it so vt100 never sees it
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    (cwds, out)
}

impl Session {
    pub fn new(id: usize, name: impl Into<String>, cols: u16, rows: u16, startup_cmds: Vec<String>) -> Self {
        let pty_system = native_pty_system();
        let (cols, rows) = (cols.max(1), rows.max(1));

        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("failed to open pty");

        let mut cmd = CommandBuilder::new("zsh");
        cmd.env("TERM", "xterm-256color");

        // Write a temp ZDOTDIR/.zshrc that installs the OSC 7 precmd hook,
        // then sources the user's real ~/.zshrc. This way the hook is loaded
        // before the first prompt with zero echo artifacts.
        let zdotdir = std::env::temp_dir().join(format!("mbulet-zdotdir-{}", id));
        let _ = std::fs::create_dir_all(&zdotdir);
        let real_zshrc = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join(".zshrc");
        let zshrc_content = format!(
            r#"# mbulet session bootstrap
__mbulet_precmd() {{ printf '\e]7;file://%s%s\a' "$(hostname -s)" "$PWD"; }}
precmd_functions+=(__mbulet_precmd)
[[ -f {real_zshrc} ]] && source {real_zshrc}
PROMPT_SP=""
"#,
            real_zshrc = real_zshrc.display()
        );
        let _ = std::fs::write(zdotdir.join(".zshrc"), &zshrc_content);
        cmd.env("ZDOTDIR", &zdotdir);

        let child = pair.slave.spawn_command(cmd).expect("failed to spawn shell");
        let mut writer = pair.master.take_writer().expect("pty writer");
        let mut reader = pair.master.try_clone_reader().expect("pty reader");

        // Send startup commands (e.g. cd to worktree dir) as normal input.
        // These are user-visible commands — echo is fine.
        for cmd in &startup_cmds {
            let _ = writer.write_all(cmd.as_bytes());
            let _ = writer.write_all(b"\n");
        }
        let _ = writer.flush();

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let parser_clone = Arc::clone(&parser);
        let output_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let output_buf_clone = Arc::clone(&output_buf);
        let subscribers: Arc<Mutex<Vec<std::sync::mpsc::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let subscribers_clone = Arc::clone(&subscribers);
        let cwd: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cwd_clone = Arc::clone(&cwd);
        let cwd_subscribers: Arc<Mutex<Vec<std::sync::mpsc::Sender<String>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let cwd_subscribers_clone = Arc::clone(&cwd_subscribers);

        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let raw = &buf[..n];
                        // Extract and strip OSC 7 before passing to vt100
                        let (cwds, clean) = extract_osc7(raw);

                        // Update CWD and notify subscribers
                        if let Some(new_cwd) = cwds.into_iter().last() {
                            *cwd_clone.lock().unwrap() = Some(new_cwd.clone());
                            let mut subs = cwd_subscribers_clone.lock().unwrap();
                            subs.retain(|tx| tx.send(new_cwd.clone()).is_ok());
                        }

                        // Feed cleaned data to vt100 parser
                        parser_clone.lock().unwrap().process(&clean);

                        // Append cleaned data to ring buffer and notify PTY subscribers
                        let mut ob = output_buf_clone.lock().unwrap();
                        ob.extend_from_slice(&clean);
                        if ob.len() > 65536 {
                            let drain = ob.len() - 65536;
                            ob.drain(..drain);
                        }
                        drop(ob);
                        let mut subs = subscribers_clone.lock().unwrap();
                        subs.retain(|tx| tx.send(clean.clone()).is_ok());
                    }
                }
            }
        });

        Self {
            id,
            name: name.into(),
            parser,
            writer,
            cwd,
            child,
            _slave: pair.slave,
            master: pair.master,
            zdotdir,
            output_buf,
            subscribers,
            cwd_subscribers,
        }
    }

    /// Used on attach: atomically reset parser + clear output_buf + resize PTY.
    /// Holds output_buf lock across the whole operation to prevent reader-thread races.
    /// Only sends SIGWINCH when dimensions actually change.
    /// Returns true if dimensions changed (server cleared buffer; client should also reset parser).
    pub fn resize_and_reset(&self, cols: u16, rows: u16) -> bool {
        let (c, r) = (cols.max(1), rows.max(1));
        let mut ob = self.output_buf.lock().unwrap();
        let mut parser = self.parser.lock().unwrap();
        let old_cols = parser.screen().size().1;
        let old_rows = parser.screen().size().0;
        let changed = c != old_cols || r != old_rows;
        if changed {
            *parser = vt100::Parser::new(r, c, 0);
            ob.clear();
            drop(parser);
            drop(ob);
            // SIGWINCH only on real size change — forces shell redraw at new size
            let _ = self.master.resize(PtySize { rows: r, cols: c, pixel_width: 0, pixel_height: 0 });
        }
        // Same size: do nothing — buffered output already has correct screen state
        changed
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let (c, r) = (cols.max(1), rows.max(1));
        self.parser.lock().unwrap().set_size(r, c);
        let _ = self.master.resize(PtySize {
            rows: r, cols: c,
            pixel_width: 0, pixel_height: 0,
        });
    }

    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    pub fn subscribe_cwd(&self) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.cwd_subscribers.lock().unwrap().push(tx);
        rx
    }

    pub fn buffered_output(&self) -> Vec<u8> {
        self.output_buf.lock().unwrap().clone()
    }

    pub fn current_cwd(&self) -> Option<String> {
        self.cwd.lock().unwrap().clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.zdotdir);
    }
}
