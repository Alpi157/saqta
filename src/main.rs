mod capture;
mod codex;
mod doctor;
mod exec;
mod filter;
mod gc;
#[cfg(debug_assertions)]
mod harness;
mod hook;
mod journal;
mod output;
mod paths;
mod restore;
mod supervisor;
mod sys;

fn main() {
    #[cfg(debug_assertions)]
    if std::env::args_os().nth(1).is_some_and(|s| s == "__harness") {
        let result = harness::run();
        if let Err(e) = &result {
            eprintln!("harness: {e:#}");
        }
        std::process::exit(if result.is_ok() { 0 } else { 125 });
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|s| s == "__supervisor")
    {
        let code = supervisor::internal(std::env::args_os().skip(2).collect()).unwrap_or(125);
        sys::exit(code);
    }
    if std::env::args_os().nth(1).is_some_and(|s| s == "hook") {
        sys::ignore_sigpipe();
        hook::run();
        return;
    }
    let undo = std::env::args_os()
        .nth(1)
        .is_some_and(|s| s == "undo" || s == "gc" || s == "init" || s == "uninstall");
    if undo {
        sys::ignore_sigpipe();
    } else if std::env::args_os().nth(1).is_none_or(|s| s != "exec") {
        sys::default_sigpipe();
    }
    let cli = clap::Command::new("saqta")
        .about("Run coding agent commands under a seccomp supervisor")
        .version(env!("CARGO_PKG_VERSION"))
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("doctor")
                .about("Check whether this environment supports Saqta")
                .arg(
                    clap::Arg::new("json")
                        .long("json")
                        .help("Print checks and advice as JSON")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
        .subcommand(
            clap::Command::new("exec")
                .about("Run a shell string or a program under supervision")
                .arg(
                    clap::Arg::new("shell")
                        .long("shell")
                        .value_name("PATH")
                        .help("Shell for -c strings (default: /bin/sh)")
                        .value_parser(clap::value_parser!(std::ffi::OsString))
                        .requires("command"),
                )
                .arg(
                    clap::Arg::new("command")
                        .short('c')
                        .value_name("STRING")
                        .help("Run STRING with the selected shell")
                        .allow_hyphen_values(true)
                        .value_parser(clap::value_parser!(std::ffi::OsString)),
                )
                .arg(
                    clap::Arg::new("argv")
                        .value_names(["PROGRAM", "ARGS"])
                        .help("Program and arguments to run directly, with PATH lookup")
                        .num_args(1..)
                        .trailing_var_arg(true)
                        .conflicts_with("command")
                        .value_parser(clap::value_parser!(std::ffi::OsString)),
                ),
        )
        .subcommand(
            clap::Command::new("log")
                .about("List captured commands")
                .arg(
                    clap::Arg::new("number")
                        .short('n')
                        .value_name("N")
                        .help("Maximum number of commands to list")
                        .default_value("20")
                        .value_parser(clap::value_parser!(usize)),
                )
                .arg(
                    clap::Arg::new("json")
                        .long("json")
                        .help("Print the command log as JSON")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
        .subcommand(
            clap::Command::new("show")
                .about("Show a command's effects and kept versions")
                .arg(
                    clap::Arg::new("id")
                        .help("Command id to inspect")
                        .required(true)
                        .value_parser(clap::value_parser!(u64)),
                )
                .arg(
                    clap::Arg::new("json")
                        .long("json")
                        .help("Print effects as JSON")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
        .subcommand(
            clap::Command::new("undo")
                .arg(
                    clap::Arg::new("resume")
                        .long("resume")
                        .help("Finish an interrupted undo from its recovery log")
                        .action(clap::ArgAction::SetTrue),
                )
                .about("Restore the filesystem before a captured command")
                .arg(
                    clap::Arg::new("id")
                        .help("Command id to undo")
                        .required(true)
                        .value_parser(clap::value_parser!(u64)),
                )
                .arg(
                    clap::Arg::new("dry-run")
                        .long("dry-run")
                        .help("Check conflicts and print changes without restoring")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    clap::Arg::new("force")
                        .long("force")
                        .help("Restore despite newer paths, keeping displaced entries")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    clap::Arg::new("verbose")
                        .long("verbose")
                        .help("Print every effect as well as the summary")
                        .action(clap::ArgAction::SetTrue),
                ),
        );
    let cli = cli.subcommand(clap::Command::new("status")).subcommand(
        clap::Command::new("gc")
            .about("Collect expired retained content")
            .arg(
                clap::Arg::new("dry-run")
                    .long("dry-run")
                    .action(clap::ArgAction::SetTrue),
            )
            .arg(
                clap::Arg::new("older-than")
                    .long("older-than")
                    .default_value("7d"),
            )
            .arg(
                clap::Arg::new("max-size")
                    .long("max-size")
                    .default_value("5GB"),
            )
            .arg(
                clap::Arg::new("json")
                    .long("json")
                    .action(clap::ArgAction::SetTrue),
            ),
    );
    let cli = ["init", "uninstall"].into_iter().fold(cli, |cli, name| {
        cli.subcommand(
            clap::Command::new(name)
                .arg(
                    clap::Arg::new("codex")
                        .long("codex")
                        .required(true)
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    clap::Arg::new("project").value_parser(clap::value_parser!(std::path::PathBuf)),
                ),
        )
    });
    let m = cli.get_matches();
    let result = match m.subcommand() {
        Some(("status", _)) => codex::status(),
        Some(("gc", m)) => gc::run(
            m.get_flag("dry-run"),
            m.get_one::<String>("older-than")
                .map_or("7d", String::as_str),
            m.get_one::<String>("max-size")
                .map_or("5GB", String::as_str),
            m.get_flag("json"),
        ),
        Some((name @ ("init" | "uninstall"), m)) => codex::configure(
            m.get_one::<std::path::PathBuf>("project"),
            name == "uninstall",
        ),
        Some(("doctor", m)) => doctor::run(m.get_flag("json")),
        Some(("exec", m)) => exec::run(m),
        Some(("log", m)) => journal::log(
            m.get_one::<usize>("number").copied().unwrap_or(20),
            m.get_flag("json"),
        ),
        Some(("show", m)) => journal::show(
            m.get_one::<u64>("id").copied().unwrap_or(0),
            m.get_flag("json"),
        ),
        Some(("undo", m)) => restore::run(
            m.get_one::<u64>("id").copied().unwrap_or(0),
            m.get_flag("dry-run"),
            m.get_flag("force"),
            m.get_flag("verbose"),
            m.get_flag("resume"),
        ),
        _ => Ok(125),
    };
    if result.is_ok()
        && matches!(
            m.subcommand_name(),
            Some("log" | "show" | "status" | "undo")
        )
        && !m
            .subcommand()
            .is_some_and(|(_, s)| s.try_get_one::<bool>("dry-run").ok().flatten() == Some(&true))
    {
        gc::maybe_gc();
    }
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if undo {
                output::Progress::new(std::io::stderr()).line(format_args!("saqta: {e:#}"));
            } else {
                eprintln!("saqta: {e:#}");
            }
            std::process::exit(125);
        }
    }
}
