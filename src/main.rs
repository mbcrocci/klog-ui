//! A live Kubernetes log viewer: workloads (deployments, pods, services…) in the sidebar, the selected one's pod logs
//! streaming in the main pane. Uses `kubectl` and the current kubeconfig context.
//!
//! `cargo run` opens the window. `cargo run -- --snap out.png --scene demo` renders sample data
//! headless, without touching the cluster.

mod app;
mod kube;
mod logs;
mod query;
mod structured;
mod view;

use rune::{AppSpec, KeyEvent, Tx};
use rune_kit::shell;

use crate::app::{App, Resources, Stream};
use crate::kube::{Kind, Resource};

fn on_key(s: &mut App, ev: &KeyEvent, _: &mut Tx) -> bool {
    if shell::is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        return true;
    }
    false
}

fn on_close(s: &mut App, _: &mut Tx) {
    // Stops kubectl rather than leaving it following logs after the window is gone.
    s.disconnect();
}

const DEMO_LINES: [&str; 7] = [
    r#"[pod/api-7d9f8b-xk2lp/api] {"level":"info","ts":"2026-11-14T10:00:01.120Z","logger":"http","msg":"listening","port":"8080"}"#,
    r#"[pod/api-7d9f8b-qq81z/api] {"level":"info","ts":"2026-11-14T10:00:03.481Z","logger":"stripe","msg":"request","URI":"/stripe/webhooks/:integrationId","method":"POST","status":200,"error":null,"latency":"375.265275ms"}"#,
    r#"[pod/api-7d9f8b-xk2lp/api] {"level":40,"time":1794650404812,"pid":1,"hostname":"api-7d9f8b-xk2lp","integration":"5f1e0a3b9c2d4e6f7a8b9c0d","attempts":3,"msg":"slow query on orders"}"#,
    r#"[pod/api-7d9f8b-qq81z/api] {"level":50,"time":1794650406002,"pid":1,"hostname":"api-7d9f8b-qq81z","integration":"5f1e0a3b9c2d4e6f7a8b9c1e","err":{"type":"Error","message":"invalid_request: Invalid Refresh Token","stack":"Error: invalid_request: Invalid Refresh Token\n    at getRefreshedIntegration (/app/dist/index.cjs:64063:13)\n    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)\n    at async updateIntegration (/app/dist/index.cjs:64190:20)"},"msg":"unable to refresh integration"}"#,
    r##"[pod/api-7d9f8b-xk2lp/api] {"message":["Extension sync published","5f1e0a3b9c2d4e6f7a8b9c2f",{"extensions":5}],"level":"INFO","timestamp":"2026-11-14T10:00:07.694Z","annotations":{"workPlanID":"5f1e0a3b9c2d4e6f7a8b9c3a","workType":"extensions.sync"},"spans":{},"fiberId":"#20"}"##,
    r#"[pod/api-7d9f8b-qq81z/api] {"time":"2026-11-14T10:00:08.490358595Z","level":"INFO","msg":"completed work plan","worker_id":"worker-1","plan":{"ID":"5f1e0a3b9c2d4e6f7a8b9c4b","PlanType":"import_orders","RetryCount":0,"Options":{"integration":"5f1e0a3b9c2d4e6f7a8b9c5c","full":true}}}"#,
    "[pod/api-7d9f8b-xk2lp/api] 10.244.0.224 - - [14/Nov/2026:10:00:09 +0000] \"GET /health HTTP/1.1\" 200 2 \"-\" \"kube-probe/1.31\"",
];

/// Sample data for headless snapshots.
fn demo(s: &mut App) {
    let d = |ns: &str, name: &str, ready: u32, desired: u32| Resource {
        kind: Kind::Deployment,
        namespace: ns.into(),
        name: name.into(),
        badge: format!("{ready}/{desired}"),
        healthy: ready >= desired,
    };
    s.context = "my-cluster".into();
    let list = vec![
        d("default", "api", 2, 2),
        d("default", "worker", 1, 2),
        d("default", "web", 3, 3),
        d("ingress-nginx", "ingress-nginx-controller", 1, 1),
        d("monitoring", "grafana", 1, 1),
    ];
    s.selected = list.first().cloned();
    s.lists.insert(Kind::Deployment, Resources::Loaded(list));
    s.stream = Stream::Live;
    for l in DEMO_LINES {
        s.logs.push(l.into(), false);
    }
}

fn demo_tree(s: &mut App) {
    demo(s);
    s.format = app::Format::Tree;
}

fn demo_pods(s: &mut App) {
    demo(s);
    let p = |ns: &str, name: &str, badge: &str, healthy| Resource {
        kind: Kind::Pod,
        namespace: ns.into(),
        name: name.into(),
        badge: badge.into(),
        healthy,
    };
    let pods = vec![
        p("default", "api-7d9f8b-qq81z", "1/1", true),
        p("default", "api-7d9f8b-xk2lp", "1/1 ↻2", true),
        p("default", "worker-5c6d7e-ab12c", "Pending", false),
        p("monitoring", "grafana-6f7a8b-zz9yx", "1/1", true),
    ];
    s.mode = Kind::Pod;
    s.selected = Some(pods[1].clone());
    s.lists.insert(Kind::Pod, Resources::Loaded(pods));
    // One pod's lines only.
    s.logs.clear();
    for l in DEMO_LINES.iter().filter(|l| l.contains("xk2lp/")) {
        s.logs.push((*l).into(), false);
    }
}

fn main() {
    rune::App::new(AppSpec {
        setup: Some(app::setup),
        poll: Some(app::poll),
        on_key: Some(on_key),
        scenes: &[("demo", demo), ("demo-tree", demo_tree), ("demo-pods", demo_pods)],
        min_size: Some((640.0, 360.0)),
        // Inter for the UI; the system's monospace font for log lines.
        families: ("Inter", "ui-monospace, JetBrains Mono, DejaVu Sans Mono, monospace"),
        ..AppSpec::new("Kube Logs", (1200.0, 760.0), app::init, view::view)
    })
    .on_close(on_close)
    .run();
}
