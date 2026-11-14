//! Talks to the cluster through `kubectl`, so the user's kubeconfig, context and auth plugins
//! (exec credentials such as `doctl`) work as they do in a terminal. Everything here runs off
//! the UI thread and hands results to an [`Inbox`], then wakes the window.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use rune::Waker;

/// The kinds of workload the sidebar lists. Each one's logs come from `kubectl logs KIND/NAME`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Deployment,
    Pod,
    Service,
    StatefulSet,
    DaemonSet,
    Job,
}

impl Kind {
    pub const ALL: [Kind; 6] = [Kind::Deployment, Kind::Pod, Kind::Service, Kind::StatefulSet, Kind::DaemonSet, Kind::Job];

    /// The plural, for the sidebar's picker and messages.
    pub fn plural(self) -> &'static str {
        match self {
            Kind::Deployment => "Deployments",
            Kind::Pod => "Pods",
            Kind::Service => "Services",
            Kind::StatefulSet => "StatefulSets",
            Kind::DaemonSet => "DaemonSets",
            Kind::Job => "Jobs",
        }
    }

    pub fn from_plural(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.plural() == s)
    }

    /// kubectl's resource name, for `get` and `logs KIND/NAME`.
    fn resource(self) -> &'static str {
        match self {
            Kind::Deployment => "deployment",
            Kind::Pod => "pod",
            Kind::Service => "service",
            Kind::StatefulSet => "statefulset",
            Kind::DaemonSet => "daemonset",
            Kind::Job => "job",
        }
    }

    /// The `custom-columns` after namespace and name; `parse` reads them back in order.
    fn columns(self) -> &'static str {
        match self {
            Kind::Deployment | Kind::StatefulSet => "A:.status.readyReplicas,B:.spec.replicas",
            Kind::DaemonSet => "A:.status.numberReady,B:.status.desiredNumberScheduled",
            Kind::Pod => "A:.status.phase,B:.status.containerStatuses[*].ready,C:.status.containerStatuses[*].restartCount",
            Kind::Service => "A:.spec.type,B:.spec.selector",
            Kind::Job => "A:.status.succeeded,B:.spec.completions,C:.status.failed",
        }
    }

    /// A row of `kubectl get` output past namespace and name, as a badge and a health flag.
    fn parse(self, cols: &[&str]) -> (String, bool) {
        let col = |i: usize| cols.get(i).copied().filter(|c| *c != "<none>");
        let num = |i: usize| col(i).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
        match self {
            Kind::Deployment | Kind::StatefulSet | Kind::DaemonSet => {
                let (ready, desired) = (num(0), num(1));
                (format!("{ready}/{desired}"), ready >= desired)
            }
            Kind::Pod => {
                let phase = col(0).unwrap_or("Unknown");
                let ready: Vec<bool> = col(1).map(|s| s.split(',').map(|r| r == "true").collect()).unwrap_or_default();
                let restarts: u32 = col(2).map(|s| s.split(',').filter_map(|n| n.parse::<u32>().ok()).sum()).unwrap_or(0);
                let up = ready.iter().filter(|r| **r).count();
                match phase {
                    "Running" => {
                        let badge = if restarts > 0 { format!("{up}/{} ↻{restarts}", ready.len()) } else { format!("{up}/{}", ready.len()) };
                        (badge, up == ready.len())
                    }
                    "Succeeded" => ("Done".into(), true),
                    other => (other.to_owned(), false),
                }
            }
            // A service without a selector has no pods, so no logs.
            Kind::Service => (col(0).unwrap_or("").to_owned(), col(1).is_some()),
            Kind::Job => {
                let (done, want, failed) = (num(0), num(1).max(1), num(2));
                (format!("{done}/{want}"), failed == 0)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resource {
    pub kind: Kind,
    pub namespace: String,
    pub name: String,
    /// Short status for the sidebar: `2/3` ready, a pod's phase, a service's type.
    pub badge: String,
    /// False when it wants attention: not fully ready, failing, or (services) without pods.
    pub healthy: bool,
}

impl Resource {
    /// `kind/namespace/name`, unique across the cluster.
    pub fn id(&self) -> String {
        format!("{}/{}/{}", self.kind.resource(), self.namespace, self.name)
    }
}

/// What background threads hand the UI thread.
pub enum Msg {
    Context(String),
    Resources { kind: Kind, list: Result<Vec<Resource>, String> },
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

/// Reads the current context and the resources of `kind` on a worker thread.
pub fn refresh(kind: Kind, inbox: Arc<Inbox>) {
    thread::spawn(move || {
        if let Ok(out) = kubectl().args(["config", "current-context"]).output()
            && out.status.success()
        {
            inbox.push(Msg::Context(String::from_utf8_lossy(&out.stdout).trim().to_owned()));
        }
        inbox.push(Msg::Resources { kind, list: list(kind) });
    });
}

fn list(kind: Kind) -> Result<Vec<Resource>, String> {
    let out = kubectl()
        .args(["get", kind.resource(), "--all-namespaces", "--no-headers", "-o"])
        .arg(format!("custom-columns=NS:.metadata.namespace,NAME:.metadata.name,{}", kind.columns()))
        .output()
        .map_err(|e| format!("couldn't run kubectl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let mut list: Vec<Resource> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let namespace = cols.next()?.to_owned();
            let name = cols.next()?.to_owned();
            let rest: Vec<&str> = cols.collect();
            let (badge, healthy) = kind.parse(&rest);
            Some(Resource { kind, namespace, name, badge, healthy })
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

/// Follows the logs of every pod and container behind `r`, starting `tail` lines back.
pub fn stream_logs(r: &Resource, tail: u32, generation: u64, inbox: Arc<Inbox>) -> Result<LogStream, String> {
    let mut child = kubectl()
        .args(["logs", "--follow", "--all-pods=true", "--all-containers=true", "--prefix=true", "--ignore-errors=true"])
        .arg(format!("--tail={tail}"))
        .arg("--max-log-requests=50")
        .args(["--namespace", &r.namespace])
        .arg(format!("{}/{}", r.kind.resource(), r.name))
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

    #[test]
    fn parses_columns() {
        assert_eq!(Kind::Deployment.parse(&["1", "2"]), ("1/2".into(), false));
        assert_eq!(Kind::Deployment.parse(&["<none>", "0"]), ("0/0".into(), true));
        assert_eq!(Kind::Pod.parse(&["Running", "true,true", "0,0"]), ("2/2".into(), true));
        assert_eq!(Kind::Pod.parse(&["Running", "true,false", "3,1"]), ("1/2 ↻4".into(), false));
        assert_eq!(Kind::Pod.parse(&["Pending", "<none>", "<none>"]), ("Pending".into(), false));
        assert_eq!(Kind::Service.parse(&["ClusterIP", "map[app:api]"]), ("ClusterIP".into(), true));
        assert_eq!(Kind::Service.parse(&["ClusterIP", "<none>"]), ("ClusterIP".into(), false));
        assert_eq!(Kind::Job.parse(&["1", "1", "<none>"]), ("1/1".into(), true));
    }

    /// Talks to the real cluster: `cargo test -- --ignored live_cluster --nocapture`.
    #[test]
    #[ignore]
    fn live_cluster() {
        let list = list(Kind::Deployment).expect("deployments");
        println!("{} deployments", list.len());
        let d = list.iter().find(|d| d.healthy && !d.badge.starts_with("0/")).expect("a running deployment");
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
