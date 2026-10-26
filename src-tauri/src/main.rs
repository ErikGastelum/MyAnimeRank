// No console window behind the app in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--serve") {
        let port = args.get(i + 1).and_then(|p| p.parse().ok()).unwrap_or(8420);
        myanimerank_lib::serve(port);
    } else {
        myanimerank_lib::run();
    }
}
