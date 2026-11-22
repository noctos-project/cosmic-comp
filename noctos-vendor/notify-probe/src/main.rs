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

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
