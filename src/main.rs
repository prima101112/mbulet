mod client;
mod daemon;
mod protocol;
mod session;

use std::{
    fs,
    io::{self},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

fn state_dir() -> PathBuf {
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    p.push(".local/share/mbulet");
    let _ = fs::create_dir_all(&p);
    p
}

fn socket_path() -> PathBuf {
    let mut p = state_dir();
    p.push("daemon.sock");
    p
}

fn pid_path() -> PathBuf {
    let mut p = state_dir();
    p.push("daemon.pid");
    p
}

fn daemon_running(socket_path: &str) -> bool {
    // Just test if we can connect to the socket — don't occupy a client slot
    UnixStream::connect(socket_path).is_ok()
}

fn spawn_daemon(socket_path: &str) {
    let exe = std::env::current_exe().expect("current exe");
    Command::new(exe)
        .arg("--daemon")
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn daemon");
}

fn run_daemon(socket_path: &str) -> io::Result<()> {
    let _ = fs::remove_file(socket_path);
    let pid = std::process::id();
    fs::write(pid_path(), format!("{}\n", pid))?;

    let d = daemon::Daemon::new(socket_path.to_string());
    d.run();
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(|s| s.as_str()) == Some("--daemon") {
        let sock = args
            .get(2)
            .map(|s| s.as_str())
            .unwrap_or("/tmp/mbulet.sock");
        return run_daemon(sock);
    }

    let sock = socket_path();
    let sock_str = sock.to_str().unwrap_or("/tmp/mbulet.sock");

    if !daemon_running(sock_str) {
        let _ = fs::remove_file(sock_str);
        eprintln!("mbulet: starting background daemon...");
        spawn_daemon(sock_str);

        let mut ready = false;
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(100));
            if daemon_running(sock_str) {
                ready = true;
                break;
            }
        }
        if !ready {
            eprintln!("mbulet: daemon failed to start");
            std::process::exit(1);
        }
    }

    client::run_client(sock_str)
}
