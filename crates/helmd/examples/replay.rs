//! Replay a raw PTY capture through the Screen model and report which
//! content lines end up duplicated in history — the exact duplication
//! bento renders. Resizes replayed at the byte offsets they happened.
//!
//!   cargo run -p helmd --example replay -- raw.bin 100x30 9647:80x24 10842:110x40

use helmd::screen::SessionScreen;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

struct Null;
impl Write for Null {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let data = std::fs::read(args.next().expect("capture path")).expect("read capture");
    let size = args.next().expect("initial COLSxROWS");
    let (c, r) = size.split_once('x').unwrap();
    let mut resizes: Vec<(usize, u16, u16)> = args
        .map(|s| {
            let (off, sz) = s.split_once(':').unwrap();
            let (c, r) = sz.split_once('x').unwrap();
            (off.parse().unwrap(), c.parse().unwrap(), r.parse().unwrap())
        })
        .collect();
    resizes.sort();

    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(Null)));
    let mut s = SessionScreen::new(c.parse().unwrap(), r.parse().unwrap(), writer);

    let mut fed = 0usize;
    for (off, cols, rows) in &resizes {
        s.advance(&data[fed..*off]);
        fed = *off;
        s.resize(*cols, *rows);
        println!("-- resized to {cols}x{rows} at offset {off}; top_line={}", s.top_line());
    }
    s.advance(&data[fed..]);

    // History + grid content, trimmed.
    let mut lines: Vec<(u64, String)> = Vec::new();
    let (from, page) = s.history_page(0, u64::MAX);
    for (i, row) in page.iter().enumerate() {
        lines.push((from + i as u64, row.text().trim_end().to_string()));
    }
    let grid_from = s.top_line();
    s.for_each_row(|line, row| {
        if line >= grid_from {
            lines.push((line, row.text().trim_end().to_string()));
        }
    });

    println!(
        "history: {} rows [{}..{}), grid top_line={}",
        page.len(),
        from,
        from + page.len() as u64,
        grid_from
    );

    // The response is "1".."120" as standalone lines (possibly indented).
    let mut seen: HashMap<u32, Vec<(u64, bool)>> = HashMap::new();
    for (line, text) in &lines {
        let t = text.trim();
        if let Ok(n) = t.parse::<u32>() {
            if (1..=120).contains(&n) {
                seen.entry(n).or_default().push((*line, *line >= grid_from));
            }
        }
    }
    let mut dup_hist = 0;
    let mut hist_and_grid = 0;
    let mut examples = Vec::new();
    for n in 1..=120u32 {
        if let Some(offs) = seen.get(&n) {
            let hist: Vec<_> = offs.iter().filter(|(_, g)| !g).collect();
            let grid: Vec<_> = offs.iter().filter(|(_, g)| *g).collect();
            if hist.len() > 1 {
                dup_hist += 1;
                if examples.len() < 8 {
                    examples.push(format!("  n={n}: history lines {:?}", hist));
                }
            }
            if !hist.is_empty() && !grid.is_empty() {
                hist_and_grid += 1;
            }
        }
    }
    println!("numbers duplicated WITHIN history: {dup_hist}/120");
    println!("numbers in history AND still on grid: {hist_and_grid}/120");
    for e in examples {
        println!("{e}");
    }

    // Show the actual duplicated region: consecutive runs of number-lines in history.
    println!("\n-- number-lines in history, in order:");
    let mut run: Vec<u32> = Vec::new();
    let mut run_start = 0u64;
    for (line, text) in lines.iter().filter(|(l, _)| *l < grid_from) {
        if let Ok(n) = text.trim().parse::<u32>() {
            if (1..=120).contains(&n) {
                if run.is_empty() {
                    run_start = *line;
                }
                run.push(n);
                continue;
            }
        }
        if run.len() > 1 {
            println!("  lines {}..: {}..{} ({} numbers)", run_start, run[0], run[run.len() - 1], run.len());
        }
        run.clear();
    }
    if run.len() > 1 {
        println!("  lines {}..: {}..{} ({} numbers)", run_start, run[0], run[run.len() - 1], run.len());
    }
}
