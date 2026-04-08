use crate::protocol::{ClientMsg, DaemonMsg, SessionInfo, recv_msg, send_msg};
use crate::session::Session;
use std::{
    io::Write,
    os::unix::net::{UnixListener, UnixStream},
    sync::{Arc, Mutex},
    thread,
};

pub struct Daemon {
    sessions: Arc<Mutex<Vec<Session>>>,
    next_id: Arc<Mutex<usize>>,
    socket_path: Arc<String>,
}

impl Daemon {
    pub fn new(socket_path: String) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(Mutex::new(1)),
            socket_path: Arc::new(socket_path),
        }
    }

    pub fn run(self) {
        // Create initial "main" session with sane default size (client will resize on attach)
        {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.push(Session::new(1, "main", 220, 50, vec![]));
            *self.next_id.lock().unwrap() = 2;
        }

        let listener = UnixListener::bind(&*self.socket_path).expect("failed to bind socket");

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let sessions = Arc::clone(&self.sessions);
                    let next_id = Arc::clone(&self.next_id);
                    let socket_path = Arc::clone(&self.socket_path);
                    thread::spawn(move || {
                        handle_client(stream, sessions, next_id, socket_path);
                    });
                }
                Err(e) => eprintln!("[daemon] accept error: {}", e),
            }
        }
    }
}

fn handle_client(
    mut stream: UnixStream,
    sessions: Arc<Mutex<Vec<Session>>>,
    next_id: Arc<Mutex<usize>>,
    socket_path: Arc<String>,
) {
    let mut attached_id: Option<usize> = None;
    // Channel for PTY output streaming to this client
    let mut pty_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>> = None;
    // Channel for CWD updates for the attached session
    let mut cwd_rx: Option<std::sync::mpsc::Receiver<String>> = None;

    // Shared write half
    let stream_write = Arc::new(Mutex::new(stream.try_clone().expect("clone stream")));

    loop {
        // If attached, drain pending PTY output to client (non-blocking)
        if let Some(rx) = &pty_rx {
            loop {
                match rx.try_recv() {
                    Ok(data) => {
                        if let Some(id) = attached_id {
                            let msg = DaemonMsg::PtyOutput { id, data };
                            let mut w = stream_write.lock().unwrap();
                            if send_msg(&mut *w, &msg).is_err() {
                                return;
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }

        // Drain pending CWD updates (non-blocking)
        if let Some(rx) = &cwd_rx {
            loop {
                match rx.try_recv() {
                    Ok(cwd) => {
                        if let Some(id) = attached_id {
                            let msg = DaemonMsg::CwdUpdate { id, cwd };
                            let mut w = stream_write.lock().unwrap();
                            if send_msg(&mut *w, &msg).is_err() {
                                return;
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }

        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(20)))
            .ok();
        let msg: Result<ClientMsg, _> = recv_msg(&mut stream);

        match msg {
            Ok(msg) => match msg {
                ClientMsg::ListSessions => {
                    let sessions = sessions.lock().unwrap();
                    let list: Vec<SessionInfo> = sessions
                        .iter()
                        .map(|s| SessionInfo {
                            id: s.id,
                            name: s.name.clone(),
                            cwd: s.current_cwd(),
                        })
                        .collect();
                    let _ = send_msg(&mut stream, &DaemonMsg::SessionList { sessions: list });
                }

                ClientMsg::NewSession { name, cols, rows, startup_cmds } => {
                    let id = {
                        let mut nid = next_id.lock().unwrap();
                        let id = *nid;
                        *nid += 1;
                        id
                    };
                    let session = Session::new(id, &name, cols, rows, startup_cmds);
                    sessions.lock().unwrap().push(session);
                    let _ = send_msg(&mut stream, &DaemonMsg::SessionCreated { id, name });
                }

                ClientMsg::DeleteSession { id } => {
                    let mut s = sessions.lock().unwrap();
                    if s.len() <= 1 {
                        let _ = send_msg(
                            &mut stream,
                            &DaemonMsg::Error {
                                msg: "cannot delete last session".into(),
                            },
                        );
                        continue;
                    }

                    let before = s.len();
                    s.retain(|sess| sess.id != id);
                    if s.len() < before {
                        let _ = send_msg(&mut stream, &DaemonMsg::SessionDeleted { id });
                        if attached_id == Some(id) {
                            attached_id = None;
                            pty_rx = None;
                            cwd_rx = None;
                            let _ = send_msg(&mut stream, &DaemonMsg::Detached);
                        }
                    } else {
                        let _ = send_msg(
                            &mut stream,
                            &DaemonMsg::Error {
                                msg: format!("session {} not found", id),
                            },
                        );
                    }
                }

                ClientMsg::RenameSession { id, name } => {
                    let mut s = sessions.lock().unwrap();
                    if let Some(sess) = s.iter_mut().find(|s| s.id == id) {
                        sess.name = name.clone();
                        let _ = send_msg(&mut stream, &DaemonMsg::SessionRenamed { id, name });
                    } else {
                        let _ = send_msg(
                            &mut stream,
                            &DaemonMsg::Error {
                                msg: format!("session {} not found", id),
                            },
                        );
                    }
                }

                ClientMsg::ReorderSession { id, new_index } => {
                    let mut s = sessions.lock().unwrap();
                    if let Some(old_index) = s.iter().position(|s| s.id == id) {
                        if new_index < s.len() {
                            let session = s.remove(old_index);
                            s.insert(new_index, session);
                            let _ = send_msg(&mut stream, &DaemonMsg::SessionReordered { id, new_index });
                        } else {
                            let _ = send_msg(
                                &mut stream,
                                &DaemonMsg::Error {
                                    msg: format!("invalid index {}", new_index),
                                },
                            );
                        }
                    } else {
                        let _ = send_msg(
                            &mut stream,
                            &DaemonMsg::Error {
                                msg: format!("session {} not found", id),
                            },
                        );
                    }
                }

                ClientMsg::Attach { id, cols, rows } => {
                    let sessions = sessions.lock().unwrap();
                    if let Some(sess) = sessions.iter().find(|s| s.id == id) {
                        attached_id = Some(id);

                        // Atomically reset parser + clear buf + resize. If dims changed,
                        // send no buffer — SIGWINCH redraws fresh. Same-size reattach
                        // sends current buffer so screen is restored.
                        let changed = sess.resize_and_reset(cols, rows);
                        if !changed {
                            let buf = sess.buffered_output();
                            if !buf.is_empty() {
                                let _ = send_msg(&mut stream, &DaemonMsg::PtyOutput { id, data: buf });
                            }
                        }

                        // Send current CWD if known
                        if let Some(cwd) = sess.current_cwd() {
                            let _ = send_msg(&mut stream, &DaemonMsg::CwdUpdate { id, cwd });
                        }

                        // Subscribe for live PTY output and CWD updates
                        pty_rx = Some(sess.subscribe());
                        cwd_rx = Some(sess.subscribe_cwd());
                        let _ = send_msg(&mut stream, &DaemonMsg::Attached { id, cleared: changed });
                    } else {
                        let _ = send_msg(
                            &mut stream,
                            &DaemonMsg::Error {
                                msg: format!("session {} not found", id),
                            },
                        );
                    }
                }

                ClientMsg::Detach => {
                    attached_id = None;
                    pty_rx = None;
                    cwd_rx = None;
                    let _ = send_msg(&mut stream, &DaemonMsg::Detached);
                }

                ClientMsg::Input { data } => {
                    if let Some(id) = attached_id {
                        let mut s = sessions.lock().unwrap();
                        if let Some(sess) = s.iter_mut().find(|s| s.id == id) {
                            let _ = sess.writer.write_all(&data);
                            let _ = sess.writer.flush();
                        }
                    }
                }

                ClientMsg::Resize { cols, rows } => {
                    if let Some(id) = attached_id {
                        let s = sessions.lock().unwrap();
                        if let Some(sess) = s.iter().find(|s| s.id == id) {
                            sess.resize(cols, rows);
                        }
                    }
                }

                ClientMsg::Shutdown => {
                    eprintln!("[daemon] shutdown requested");
                    // Drop all sessions first — this kills all child processes via Session::drop
                    sessions.lock().unwrap().clear();
                    // Confirm to client before exiting
                    let _ = send_msg(&mut stream, &DaemonMsg::Ok);
                    // Clean up socket and pid file
                    let _ = std::fs::remove_file(&*socket_path);
                    let pid_path = std::path::Path::new(&*socket_path)
                        .parent()
                        .map(|p| p.join("daemon.pid"));
                    if let Some(p) = pid_path {
                        let _ = std::fs::remove_file(p);
                    }
                    std::process::exit(0);
                }
            },
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                {
                    continue;
                }
                eprintln!("[daemon] client disconnected: {}", e);
                return;
            }
        }
    }
}

