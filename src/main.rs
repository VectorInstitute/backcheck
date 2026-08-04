//! backcheck — verify what your coding agent actually did.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use backcheck::{analyse_transcript, hook, report, session, verify};

#[derive(Parser)]
#[command(
    name = "backcheck",
    version,
    about = "Verify what your coding agent actually did",
    long_about = "backcheck reads a Claude Code session transcript and checks the agent's claims \
                  — \"tests pass\", \"committed\", \"created X\" — against the evidence of what it \
                  actually ran. No model calls: every verdict comes from recorded output."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Analyse a specific transcript instead of the most recent session.
    #[arg(long, short = 'f', global = true, value_name = "FILE")]
    transcript: Option<PathBuf>,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    /// Show supported claims as well as problems.
    #[arg(long, short, global = true)]
    verbose: bool,

    /// Also show what backcheck recognised, and what ran that it did not.
    #[arg(long, global = true)]
    explain: bool,

    /// Also consult the working tree and git repository, not just the transcript.
    #[arg(long, global = true)]
    live: bool,

    /// Never use colour.
    #[arg(long, global = true)]
    no_color: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Check the most recent session for this directory (the default).
    Check {
        /// Check the newest session across all projects, not just this directory.
        #[arg(long)]
        any_project: bool,
    },
    /// Run as a Claude Code Stop hook, reading its payload from stdin.
    Hook {
        /// Report findings without blocking the turn.
        #[arg(long)]
        no_block: bool,
    },
    /// Add backcheck to Claude Code's Stop hooks.
    Install {
        /// Install for every project (~/.claude/settings.json) instead of this one.
        #[arg(long)]
        global: bool,
        /// Block the turn when a claim is not supported, instead of only reporting.
        #[arg(long)]
        block: bool,
    },
    /// Show a worked example on a recorded session, with nothing of yours involved.
    Demo,
    /// Check this project's recent sessions, not just the last one.
    History {
        /// How many of the most recent sessions to read.
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Remove backcheck from Claude Code's Stop hooks.
    Uninstall {
        /// Remove the global installation.
        #[arg(long)]
        global: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("backcheck: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode> {
    match &cli.command {
        Some(Command::Hook { no_block }) => run_hook(cli, *no_block),
        Some(Command::Install { global, block }) => {
            let path = hook::install(*global, *block)?;
            println!("backcheck installed as a Stop hook in {}", path.display());
            println!(
                "{}",
                if *block {
                    "When a claim is not supported, Claude will be asked to resolve it before finishing."
                } else {
                    "It will report what it could not verify, without interrupting the turn.\nRun `backcheck install --block` if you would rather it stop and ask."
                }
            );
            println!(
                "\nRun `backcheck uninstall{}` to remove it.",
                if *global { " --global" } else { "" }
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Uninstall { global }) => {
            match hook::uninstall(*global)? {
                Some(p) => println!("backcheck removed from {}", p.display()),
                None => println!("backcheck was not installed there; nothing to do."),
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Demo) => run_demo(cli),
        Some(Command::History { limit }) => run_history(cli, *limit),
        Some(Command::Check { any_project }) => run_check(cli, *any_project),
        None => run_check(cli, false),
    }
}

fn run_check(cli: &Cli, any_project: bool) -> Result<ExitCode> {
    let path = match &cli.transcript {
        Some(p) => p.clone(),
        None => {
            let cwd = std::env::current_dir().ok();
            let scoped = if any_project { None } else { cwd.as_deref() };
            session::latest_transcript(scoped).or_else(|e| {
                // A session started elsewhere is better than no answer at all.
                if any_project {
                    Err(e)
                } else {
                    session::latest_transcript(None).map_err(|_| e)
                }
            })?
        }
    };

    let opts = verify::Options {
        live: cli.live,
        cwd: std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string()),
    };
    let report = analyse_transcript(&path, &opts)
        .with_context(|| format!("analysing {}", path.display()))?;

    if cli.json {
        println!("{}", report::to_json(&report));
    } else {
        print!(
            "{}",
            report::to_terminal(&report, use_color(cli), cli.verbose)
        );
        if cli.explain {
            print!("{}", report::to_explanation(&report, use_color(cli)));
        }
    }

    Ok(if report.has_problems() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// A worked example, so the tool can be judged before it is trusted with anything real.
///
/// The transcript is compiled in rather than read from disk: `cargo install` strips the test
/// fixtures from the published crate, and a demo that only works from a git checkout is not a
/// demo.
fn run_demo(cli: &Cli) -> Result<ExitCode> {
    const DEMO: &str = include_str!("../tests/fixtures/demo.jsonl");
    let transcript = backcheck::transcript::Transcript::parse_str(DEMO);
    let report = backcheck::analyse(
        &transcript,
        "a recorded session (backcheck demo)".to_string(),
        &verify::Options::default(),
    );

    if cli.json {
        println!("{}", report::to_json(&report));
        return Ok(ExitCode::SUCCESS);
    }

    print!(
        "{}",
        report::to_terminal(&report, use_color(cli), cli.verbose)
    );
    if cli.explain {
        print!("{}", report::to_explanation(&report, use_color(cli)));
    }
    println!(
        "  In that session the agent hit a failing test, skipped it, re-ran only that one file,\n  and reported success. Run `backcheck` in your own project to check your last session.\n"
    );
    Ok(ExitCode::SUCCESS)
}

/// Read the project's recent sessions and report only the ones with something to show.
///
/// A single session is a thin first impression: most sessions are honest, so the newest one
/// often has nothing to say. Looking back over a project's history is what makes the tool's
/// value visible immediately, and it costs milliseconds per session.
fn run_history(cli: &Cli, limit: usize) -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let paths = session::transcripts_for(&cwd)?;
    if paths.is_empty() {
        println!(
            "No Claude Code sessions recorded for {}.\n\nRun backcheck from a directory where you have used Claude Code, \nor point it at a transcript with --transcript <file>.",
            cwd.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let opts = verify::Options {
        live: cli.live,
        cwd: Some(cwd.display().to_string()),
    };

    let mut examined = 0usize;
    let mut flagged = Vec::new();
    for path in paths.iter().take(limit) {
        let Ok(report) = analyse_transcript(path, &opts) else {
            continue;
        };
        examined += 1;
        if report.has_problems() {
            flagged.push(report);
        }
    }

    let color = use_color(cli);
    for report in &flagged {
        print!("{}", report::to_terminal(report, color, cli.verbose));
    }

    let sessions = |n: usize| format!("{n} session{}", if n == 1 { "" } else { "s" });
    if flagged.is_empty() {
        println!(
            "\nRead {}. Nothing in them went unsupported.\n",
            sessions(examined)
        );
    } else {
        println!(
            "{} of {} had something worth a look.\n",
            sessions(flagged.len()),
            examined
        );
    }

    Ok(if flagged.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn run_hook(cli: &Cli, no_block: bool) -> Result<ExitCode> {
    let input = hook::HookInput::from_stdin()?;

    let path = match input.transcript().or_else(|| cli.transcript.clone()) {
        Some(p) => p,
        None => match session::latest_transcript(None) {
            Ok(p) => p,
            // A hook that cannot find a transcript must not break the user's session.
            Err(_) => {
                println!("{}", hook::allow_output());
                return Ok(ExitCode::SUCCESS);
            }
        },
    };

    let opts = verify::Options {
        live: cli.live,
        cwd: input.cwd.clone(),
    };

    // This runs inside someone's editing session. A malformed transcript or a bug in the analysis
    // must degrade to "say nothing and let the turn finish", never to a broken session.
    let analysed = std::panic::catch_unwind(|| analyse_transcript(&path, &opts));
    let report = match analysed {
        Ok(Ok(r)) => r,
        _ => {
            println!("{}", hook::allow_output());
            return Ok(ExitCode::SUCCESS);
        }
    };

    // Blocking again while the model is already answering a block risks a loop.
    if report.has_problems() && !no_block && !input.stop_hook_active {
        println!("{}", hook::block_output(&report::to_hook_reason(&report)));
        return Ok(ExitCode::SUCCESS);
    }

    if report.has_problems() {
        eprintln!("{}", report::to_terminal(&report, false, false));
    }
    println!("{}", hook::allow_output());
    Ok(ExitCode::SUCCESS)
}

/// Respect `NO_COLOR`, `--no-color`, and non-terminal output.
fn use_color(cli: &Cli) -> bool {
    if cli.no_color || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::env::var_os("TERM").is_some_and(|t| t != "dumb")
}
