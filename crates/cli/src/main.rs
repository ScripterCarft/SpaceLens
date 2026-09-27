//! `spacelens-cli` – headless scanner for benchmarking and scripting.
//!
//! ```text
//! spacelens-cli <PFAD> [--method auto|walk|mft] [--threads N] [--top N] [--quiet]
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use spacelens_core::query::{Sort, sorted_children};
use spacelens_core::{NodeId, ScanOptions, ScanProgress, Tree, format};

struct Args {
    root: PathBuf,
    threads: usize,
    top: usize,
    quiet: bool,
    method: spacelens_win::Method,
}

const USAGE: &str = "Verwendung: spacelens-cli <PFAD> [--method auto|walk|mft] [--threads N] [--top N] [--quiet]";

fn parse_args() -> Result<Args, String> {
    let mut args =
        Args { root: PathBuf::new(), threads: 0, top: 15, quiet: false, method: spacelens_win::Method::Auto };
    let mut it = std::env::args_os().skip(1);
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--threads") => args.threads = number(it.next())?,
            Some("--top") => args.top = number(it.next())?,
            Some("--quiet") => args.quiet = true,
            Some("--method") => {
                args.method = match it.next().as_ref().and_then(|m| m.to_str()) {
                    Some("auto") => spacelens_win::Method::Auto,
                    Some("walk") => spacelens_win::Method::Walk,
                    Some("mft") => spacelens_win::Method::Mft,
                    _ => return Err(format!("--method erwartet auto, walk oder mft\n{USAGE}")),
                }
            }
            Some("-h" | "--help") => return Err(USAGE.into()),
            _ if args.root.as_os_str().is_empty() => args.root = PathBuf::from(a),
            _ => return Err(format!("Unbekanntes Argument {a:?}\n{USAGE}")),
        }
    }
    if args.root.as_os_str().is_empty() {
        return Err(USAGE.into());
    }
    Ok(args)
}

fn number(v: Option<std::ffi::OsString>) -> Result<usize, String> {
    v.and_then(|s| s.to_str()?.parse().ok()).ok_or_else(|| format!("Zahl erwartet\n{USAGE}"))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let elevated = spacelens_win::elevation::is_elevated();
    let backup = elevated && spacelens_win::elevation::enable_backup_privilege();

    let progress = Arc::new(ScanProgress::new());
    let scanner = spacelens_win::scanner(args.method);
    let options = ScanOptions { threads: args.threads };

    let worker = {
        let progress = Arc::clone(&progress);
        let root = args.root.clone();
        std::thread::spawn(move || scanner.scan(&root, &options, &progress))
    };

    if !args.quiet {
        while !worker.is_finished() {
            let s = progress.snapshot();
            eprint!(
                "\r{:>12} Dateien  {:>10} Ordner  {:>10}   ",
                format::count(s.files),
                format::count(s.dirs),
                format::bytes(s.bytes)
            );
            let _ = std::io::stderr().flush();
            std::thread::sleep(Duration::from_millis(200));
        }
        eprint!("\r{:80}\r", "");
    }

    let outcome = match worker.join() {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            eprintln!("Fehler: {e}");
            return ExitCode::FAILURE;
        }
        Err(_) => {
            eprintln!("Fehler: Scan-Thread abgestürzt");
            return ExitCode::FAILURE;
        }
    };

    let t = &outcome.tree;
    let root = t.node(Tree::ROOT);
    let (files, dirs) = t.counts(Tree::ROOT);
    let secs = outcome.elapsed.as_secs_f64();
    let entries = t.len() as f64;

    println!("Pfad          {}", t.root_path());
    println!("Methode       {} ({} Threads)", outcome.method, outcome.threads);
    println!(
        "Rechte        {}",
        match (elevated, backup) {
            (true, true) => "Administrator + SeBackupPrivilege",
            (true, false) => "Administrator",
            _ => "Standardbenutzer (geschützte Ordner fehlen)",
        }
    );
    println!("Dauer         {}", format::seconds(secs));
    println!(
        "Einträge      {} ({} Dateien, {} Ordner)",
        format::count(t.len() as u64),
        format::count(u64::from(files)),
        format::count(u64::from(dirs))
    );
    println!("Durchsatz     {} Einträge/s", format::count((entries / secs.max(1e-9)) as u64));
    println!("Größe         {} (belegt {})", format::bytes(root.size), format::bytes(root.alloc));
    println!("Fehler        {}", format::count(t.issues().len() as u64));
    println!(
        "Speicher      {} ({:.1} Bytes/Eintrag)",
        format::bytes(t.memory_bytes() as u64),
        t.memory_bytes() as f64 / entries.max(1.0)
    );
    if outcome.cancelled {
        println!("Status        ABGEBROCHEN");
    }

    if args.top > 0 {
        println!("\nGrößte Einträge in {}:", t.root_path());
        for id in sorted_children(t, Tree::ROOT, Sort::SIZE_DESC, false).into_iter().take(args.top) {
            print_row(t, id, root.size);
        }
        println!("\nGrößte Dateien:");
        for id in largest_files(t, args.top) {
            print_row(t, id, root.size);
        }
    }
    ExitCode::SUCCESS
}

fn print_row(t: &Tree, id: NodeId, total: u64) {
    let n = t.node(id);
    println!(
        "  {:>10}  {:>7}  {}{}",
        format::bytes(n.size),
        format::percent(spacelens_core::query::fraction(n.size, total)),
        t.path(id),
        if n.is_dir() { "\\" } else { "" }
    );
}

/// Top-`k` files by size using a bounded min-heap: O(n log k), no full sort.
fn largest_files(t: &Tree, k: usize) -> Vec<NodeId> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let mut heap = BinaryHeap::with_capacity(k + 1);
    for id in 0..t.len() as NodeId {
        let n = t.node(id);
        if n.is_dir() {
            continue;
        }
        heap.push(Reverse((n.size, id)));
        if heap.len() > k {
            heap.pop();
        }
    }
    let mut out: Vec<_> = heap.into_iter().map(|Reverse(x)| x).collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out.into_iter().map(|(_, id)| id).collect()
}
