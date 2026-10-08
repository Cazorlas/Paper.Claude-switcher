use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use std::sync::mpsc;

/// `Command::output` with a deadline: the child is killed once `timeout`
/// passes and the call returns `TimedOut`. The deadline also covers collecting
/// the output: a daemon that outlives the direct child can inherit the pipe
/// write handles (on Windows, through the node process behind `codex.cmd`),
/// so EOF may never arrive. The exit status is what matters then, so whatever
/// was not delivered in time is dropped.
pub(crate) fn output_with_timeout(mut command: Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain both pipes on their own threads so a chatty child cannot block
    // on a full pipe while it is being waited on. The threads are never
    // joined: a reader stuck on a pipe that stays open is left to detach.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("codex did not answer within {}s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let collect = |rx: mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    };
    Ok(Output {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}
