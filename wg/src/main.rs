//! wg: the deterministic half of workgraph. Every subcommand prints one JSON line on stdout.
//! Run it from the repo root.
//!
//!   wg recall
//!   wg check --nonce N [--timeout-secs S] --cmd "npm test" [--cmd …]
//!   wg save < memory.json
//!   wg finalize --task T --by NAME --cli /abs/haruspex.py --message M --evidence-message M2 [--exclude-json '[...]']

use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match wg::cli::legacy::run(&args) {
        Ok(v) => println!("{v}"),
        Err(e) => {
            println!("{}", json!({ "error": e }));
            std::process::exit(2);
        }
    }
}
