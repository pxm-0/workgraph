//! wg: the workgraph kernel. Every subcommand prints one JSON line on stdout and exits 2 on error,
//! except `wg guard` (a PreToolUse hook: exit 0 allows, 2 denies) and `wg auth key` (an
//! `apiKeyHelper`: the key alone on stdout). Run it from the repo root.
//!
//!   wg run --experimental --task-file F [--task-id T] [--initialized] [--user-changes-file F] [--decisions-file F]
//!   wg finalize --run R --by NAME --message M --evidence-message M2
//!   wg config allow-check --repo SLUG --name N --cmd C --factory-cmd C2 | pin-haruspex --cli /abs
//!   wg doctor [--live] | wg memory import | wg guard --manifest P | wg auth key PROVIDER
//!   wg shift run | resume ID | status | report [ID] | recover | gc | schedule-print
//!   wg decide --repo R --issue N --decisions-file F | wg approve --repo R --pr N --by NAME
//!   wg approval-summary --repo R --pr N
//!   wg sandbox doctor | reap | setup-print | exec (--unit U [--patch] | --task T) -- CMD... |
//!     exec --task T --check NAME | poll --job J        wg sandbox-agent ... (linux, on the sim)
//!   wg self gate --pr N | install --sha S | rollback | pause | resume [--enable-merge] | status
//!   wg eval replay [--cases D --manifest F] | live --pr N | capture --pr URL [--expect S]
//!   wg recall | check | save | finalize (legacy, unchanged)

use serde_json::{json, Value};

fn print(result: Result<Value, String>) {
    match result {
        Ok(v) => println!("{v}"),
        Err(e) => {
            println!("{}", json!({ "error": e }));
            std::process::exit(2);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("guard") => std::process::exit(wg::cli::guard::main(rest)),
        Some("sandbox-agent") => std::process::exit(wg::sandbox::agent::main(rest)),
        Some("sandbox") => std::process::exit(wg::cli::sandbox::main(rest)),
        Some("auth") => match wg::cli::admin::auth(rest) {
            Ok(key) => println!("{key}"),
            Err(e) => {
                eprintln!("wg auth: {e}");
                std::process::exit(2);
            }
        },
        Some("run") => print(wg::cli::run::run(rest)),
        Some("finalize") if rest.iter().any(|a| a == "--run") => {
            print(wg::cli::finalize::run(rest))
        }
        Some("config") => print(wg::cli::admin::config(rest)),
        Some("doctor") => print(wg::cli::doctor::doctor(rest)),
        Some("memory") => print(wg::cli::admin::memory(rest)),
        Some("shift") => print(wg::cli::shift::shift(rest)),
        Some("decide") => print(wg::cli::shift::decide(rest)),
        Some("approve") => print(wg::cli::shift::approve(rest)),
        Some("approval-summary") => print(wg::cli::shift::approval_summary(rest)),
        Some("self") => print(wg::cli::self_::self_(rest)),
        Some("eval") => print(wg::cli::self_::eval(rest)),
        _ => print(wg::cli::legacy::run(&args)),
    }
}
