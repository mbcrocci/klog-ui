//! Talks to the cluster through `kubectl`, so the user's kubeconfig, context and auth plugins
//! (exec credentials such as `doctl`) work as they do in a terminal. Everything here runs off
//! the UI thread and hands results to an [`Inbox`], then wakes the window.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use rune::Waker;

#[derive(Clone, Debug, PartialEq)]
pub struct Deployment {
    pub namespace: String,
    pub name: String,
    pub ready: u32,
    pub desired: u32,
}

impl Deployment {
    /// `namespace/name`, unique across the cluster.
    pub fn id(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

/// What background threads hand the UI thread.
pub enum Msg {
    Context(String),
    Deployments(Result<Vec<Deployment>, String>),
    /// A line of the log stream started as generation `gen`.
    Line { generation: u64, text: String, stderr: bool },
    /// The stream of generation `gen` ended, with kubectl's exit description.
    Ended { generation: u64, status: String },
}

/// A queue background threads push into; `poll` drains it on the UI thread.
#[derive(Default)]
pub struct Inbox {
    queue: Mutex<Vec<Msg>>,
    waker: Mutex<Option<Waker>>,
}

impl Inbox {
    pub fn set_waker(&self, waker: Waker) {
        *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(waker);
    }

    pub fn push(&self, msg: Msg) {
        let was_empty = {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            let was_empty = q.is_empty();
            q.push(msg);
            was_empty
        };
        // One wake per batch: the UI drains everything queued when it polls.
        if was_empty && let Some(w) = self.waker.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            w();
        }
    }

    pub fn drain(&self) -> Vec<Msg> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

fn kubectl() -> Command {
    let mut c = Command::new(std::env::var("KUBECTL").unwrap_or_else(|_| "kubectl".into()));
    c.stdin(Stdio::null());
    c
}

/// Reads the current context and the deployments on a worker thread.
pub fn refresh(inbox: Arc<Inbox>) {
    thread::spawn(move || {
        if let Ok(out) = kubectl().args(["config", "current-context"]).output()
            && out.status.success()
        {
            inbox.push(Msg::Context(String::from_utf8_lossy(&out.stdout).trim().to_owned()));
        }
        inbox.push(Msg::Deployments(list_deployments()));
    });
}

fn list_deployments() -> Result<Vec<Deployment>, String> {
    let out = kubectl()
        .args([
            "get",
            "deployments",
            "--all-namespaces",
            "--no-headers",
            "-o",
            "custom-columns=NS:.metadata.namespace,NAME:.metadata.name,READY:.status.readyReplicas,DESIRED:.spec.replicas",
        ])
        .output()
        .map_err(|e| format!("couldn't run kubectl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let num = |s: Option<&str>| s.and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut list: Vec<Deployment> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let namespace = cols.next()?.to_owned();
            let name = cols.next()?.to_owned();
            Some(Deployment { namespace, name, ready: num(cols.next()), desired: num(cols.next()) })
        })
        .collect();
    list.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(list)
}

/// A running `kubectl logs -f`. Dropping it stops kubectl.
pub struct LogStream {
    child: Arc<Mutex<Child>>,
}

impl Drop for LogStream {
    fn drop(&mut self) {
        let mut c = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Follows the logs of every pod and container of a deployment, starting `tail` lines back.
pub fn stream_logs(d: &Deployment, tail: u32, generation: u64, inbox: Arc<Inbox>) -> Result<LogStream, String> {
    let mut child = kubectl()
        .args(["logs", "--follow", "--all-pods=true", "--all-containers=true", "--prefix=true", "--ignore-errors=true"])
        .arg(format!("--tail={tail}"))
        .arg("--max-log-requests=50")
        .args(["--namespace", &d.namespace])
        .arg(format!("deployment/{}", d.name))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run kubectl: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));

    if let Some(err) = stderr {
        let inbox = inbox.clone();
        thread::spawn(move || {
            for text in BufReader::new(err).lines().map_while(Result::ok) {
                inbox.push(Msg::Line { generation, text, stderr: true });
            }
        });
    }
    if let Some(out) = stdout {
        let child = child.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(out);
            let mut buf = Vec::new();
            // Lines are read as bytes so one invalid UTF-8 byte doesn't end the stream.
            while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                let text = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_owned();
                inbox.push(Msg::Line { generation, text, stderr: false });
                buf.clear();
            }
            // Poll for the exit without holding the lock, so `Drop` can always kill.
            let status = loop {
                match child.lock().unwrap_or_else(|e| e.into_inner()).try_wait() {
                    Ok(Some(s)) if s.success() => break "stream closed".to_owned(),
                    Ok(Some(s)) => break format!("kubectl exited ({s})"),
                    Ok(None) => {}
                    Err(e) => break format!("kubectl failed: {e}"),
                }
                thread::sleep(std::time::Duration::from_millis(50));
            };
            inbox.push(Msg::Ended { generation, status });
        });
    }
    Ok(LogStream { child })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Talks to the real cluster: `cargo test -- --ignored live_cluster --nocapture`.
    #[test]
    #[ignore]
    fn live_cluster() {
        let list = list_deployments().expect("deployments");
        println!("{} deployments", list.len());
        let d = list.iter().find(|d| d.ready > 0).expect("a running deployment");
        let inbox = Arc::new(Inbox::default());
        let stream = stream_logs(d, 5, 1, inbox.clone()).expect("stream");
        let start = Instant::now();
        let mut lines = 0;
        while start.elapsed() < Duration::from_secs(8) && lines < 5 {
            for m in inbox.drain() {
                if let Msg::Line { text, stderr, .. } = m {
                    println!("{} {}", if stderr { "!" } else { " " }, text.chars().take(140).collect::<String>());
                    lines += 1;
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
        drop(stream);
        println!("{} lines from {}", lines, d.id());
    }
}
