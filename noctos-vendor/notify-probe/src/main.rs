use notify::{RecursiveMode, Watcher};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

fn drain(rx: &mpsc::Receiver<notify::Result<notify::Event>>, root: &Path, wait_ms: u64) -> usize {
    let mut n = 0;
    // First event may take a moment; after that a short quiet period ends the step.
    let mut timeout = Duration::from_millis(wait_ms);
    while let Ok(res) = rx.recv_timeout(timeout) {
        match res {
            Ok(event) => {
                let paths: Vec<String> = event
                    .paths
                    .iter()
                    .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string())
                    .collect();
                println!("      {:?} {:?}", event.kind, paths);
            }
            Err(err) => println!("      ERROR {err}"),
        }
        n += 1;
        timeout = Duration::from_millis(250);
    }
    if n == 0 {
        println!("      (no events)");
    }
    n
}

/// `notify-probe watch SECONDS DIR...`: watch real directories for a while and
/// print every event, then count the ones cosmic-config would act on (it drops
/// access and metadata events, `Name(Both)`, and `.atomicwrite*` temp names).
/// One such event per rewritten key is what inotify produces on Linux.
fn watch_mode(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use notify::event::{EventKind, ModifyKind, RenameMode};
    use std::collections::BTreeMap;
    use std::time::Instant;

    let secs: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(20);
    let dirs: Vec<PathBuf> = args.iter().skip(1).map(PathBuf::from).collect();
    if dirs.is_empty() {
        return Err("usage: notify-probe watch SECONDS DIR...".into());
    }
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    println!("watcher kind: {:?}", notify::RecommendedWatcher::kind());
    for dir in &dirs {
        watcher.watch(dir, RecursiveMode::Recursive)?;
        println!("watching {}", dir.display());
    }
    println!("--- events for {secs} s");

    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    let mut total = 0usize;
    let mut acted_on: BTreeMap<String, usize> = BTreeMap::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(res) = rx.recv_timeout(left) else { break };
        let event = match res {
            Ok(event) => event,
            Err(err) => {
                println!("ERROR {err}");
                continue;
            }
        };
        total += 1;
        let ignored_kind = matches!(
            event.kind,
            EventKind::Access(_)
                | EventKind::Modify(ModifyKind::Metadata(_))
                | EventKind::Modify(ModifyKind::Name(RenameMode::Both))
        );
        for path in &event.paths {
            let rel = dirs
                .iter()
                .find_map(|d| path.strip_prefix(d).ok().map(|r| (d, r)))
                .map(|(d, r)| {
                    let dir = d.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                    format!("{dir}/{}", r.display())
                })
                .unwrap_or_else(|| path.display().to_string());
            let temp = path
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with(".atomicwrite"));
            let acts = !ignored_kind && !temp;
            println!(
                "{:8.3} {} {:?} {rel}",
                start.elapsed().as_secs_f64(),
                if acts { "ACT " } else { "skip" },
                event.kind
            );
            if acts {
                *acted_on.entry(rel).or_default() += 1;
            }
        }
    }
    println!("--- summary");
    println!("events received: {total}");
    println!(
        "events cosmic-config would act on: {} over {} distinct paths",
        acted_on.values().sum::<usize>(),
        acted_on.len()
    );
    let repeats: Vec<_> = acted_on.iter().filter(|(_, n)| **n > 1).collect();
    println!("paths acted on more than once: {}", repeats.len());
    for (path, n) in repeats.iter().take(20) {
        println!("  {n}x {path}");
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("watch") {
        return watch_mode(&args[1..]);
    }
    let wait_ms: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1500);
    let root: PathBuf = std::env::temp_dir().join(format!("notify-probe-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("sub"))?;
    fs::write(root.join("existing"), "one")?;
    fs::write(root.join("sub/nested"), "one")?;
    // canonical path, so reported paths can be shortened
    let root = fs::canonicalize(&root)?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    println!("watcher kind: {:?}", notify::RecommendedWatcher::kind());
    println!("root: {}", root.display());

    let step = |name: &str, op: &mut dyn FnMut() -> std::io::Result<()>| {
        println!("{name}");
        if let Err(err) = op() {
            println!("      operation failed: {err}");
        }
        drain(&rx, &root, wait_ms);
    };

    watcher.watch(&root, RecursiveMode::Recursive)?;
    step(" 0 nothing (expect no events)", &mut || Ok(()));
    step(" 1 create file 'new'", &mut || fs::write(root.join("new"), "x"));
    step(" 2 rewrite 'existing' in place", &mut || fs::write(root.join("existing"), "two"));
    step(" 3 rewrite 'existing' again", &mut || fs::write(root.join("existing"), "three"));
    step(" 4 atomic replace of 'existing' (write .tmp, rename over)", &mut || {
        fs::write(root.join(".tmp"), "four")?;
        fs::rename(root.join(".tmp"), root.join("existing"))
    });
    step(" 5 atomic replace of 'existing' again", &mut || {
        fs::write(root.join(".tmp"), "five")?;
        fs::rename(root.join(".tmp"), root.join("existing"))
    });
    step(" 6 rewrite 'existing' in place after the replaces", &mut || {
        fs::write(root.join("existing"), "six")
    });
    step(" 7 rewrite 'sub/nested'", &mut || fs::write(root.join("sub/nested"), "two"));
    step(" 8 chmod 'new'", &mut || {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("new"), fs::Permissions::from_mode(0o600))
    });
    step(" 9 rename 'new' -> 'renamed'", &mut || fs::rename(root.join("new"), root.join("renamed")));
    step("10 delete 'renamed'", &mut || fs::remove_file(root.join("renamed")));
    step("11 delete and recreate 'existing' at once", &mut || {
        fs::remove_file(root.join("existing"))?;
        fs::write(root.join("existing"), "seven")
    });
    step("12 rewrite 'existing' after the recreate", &mut || fs::write(root.join("existing"), "eight"));
    step("13 mkdir 'dir2' and create 'dir2/inner' at once", &mut || {
        fs::create_dir(root.join("dir2"))?;
        fs::write(root.join("dir2/inner"), "x")
    });
    step("14 rewrite 'dir2/inner'", &mut || fs::write(root.join("dir2/inner"), "y"));
    step("15 remove 'sub' recursively", &mut || fs::remove_dir_all(root.join("sub")));

    watcher.unwatch(&root)?;
    step("16 after unwatch: rewrite 'existing' (expect no events)", &mut || {
        fs::write(root.join("existing"), "nine")
    });

    drop(watcher);
    let _ = fs::remove_dir_all(&root);
    println!("done");
    Ok(())
}
