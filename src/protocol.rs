use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: usize,
    pub name: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    ListSessions,
    NewSession { name: String, cols: u16, rows: u16, startup_cmds: Vec<String> },
    DeleteSession { id: usize },
    RenameSession { id: usize, name: String },
    Attach { id: usize, cols: u16, rows: u16 },
    Detach,
    Input { data: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonMsg {
    SessionList { sessions: Vec<SessionInfo> },
    SessionCreated { id: usize, name: String },
    SessionDeleted { id: usize },
    SessionRenamed { id: usize, name: String },
    /// Raw PTY bytes — client feeds into its own vt100 parser
    PtyOutput { id: usize, data: Vec<u8> },
    /// CWD changed for a session (parsed from OSC 7)
    CwdUpdate { id: usize, cwd: String },
    /// cleared: true means server wiped the buffer+parser (client should also reset its parser)
    Attached { id: usize, cleared: bool },
    Detached,
    Ok,
    Error { msg: String },
}

/// Write a length-prefixed JSON message to a Write stream
pub fn send_msg<W: std::io::Write, T: Serialize>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let json = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    let len = json.len() as u32;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&json)?;
    w.flush()
}

/// Read a length-prefixed JSON message from a Read stream
pub fn recv_msg<R: std::io::Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(std::io::Error::other)
}
