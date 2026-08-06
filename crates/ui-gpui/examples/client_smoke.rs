//! Headless smoke test for the daemon client: handshake, ledger replay, and a
//! `project.list` round trip against a running `autoharnessd`. No GPU, no
//! window, and deliberately no `run.start` — starting a run would spawn a real
//! provider CLI and spend tokens.
//!
//! ```sh
//! cargo run -p autoharnessd &
//! cargo run -p autoharness-ui --example client_smoke
//! ```

use std::time::{Duration, Instant};

use autoharness_ui_gpui::client::DaemonClient;

fn main() {
    let client = DaemonClient::spawn();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        {
            let state = client.state.lock().unwrap();
            if state.connected {
                println!("connected: {}", state.status);
                break;
            }
            if Instant::now() > deadline {
                println!("NOT CONNECTED: {}", state.status);
                std::process::exit(1);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Give replay and engine detection a moment to land. Detection spawns the
    // real CLIs to ask whether the saved login is visible.
    let deadline = Instant::now() + Duration::from_secs(60);
    while client.state.lock().unwrap().engines.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }

    let state = client.state.lock().unwrap();
    println!("projects: {}", state.projects.len());
    for project in &state.projects {
        println!("  {} {}", project.name, project.path);
    }
    println!("engines: {}", state.engines.len());
    for engine in &state.engines {
        println!(
            "  {} {}",
            if engine.ready { "✓" } else { "✕" },
            engine.summary()
        );
    }
    println!("selected engine: {}", state.engine);
    println!("replayed events: {}", state.events.len());
    for line in state.events.iter().rev().take(5).collect::<Vec<_>>() {
        println!("  {line}");
    }
}
