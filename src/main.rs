use serde_json as _;

#[allow(dead_code)]
mod filter;
#[allow(dead_code)]
mod sys;

fn main() {
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
        );
    let m = cli.get_matches();
    let result: anyhow::Result<i32> = match m.subcommand() {
        Some(("doctor", _)) => Err(anyhow::anyhow!("doctor is not implemented yet")),
        Some(("exec", _)) => Err(anyhow::anyhow!("exec is not implemented yet")),
        _ => Ok(125),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("saqta: {e:#}");
            std::process::exit(125);
        }
    }
}
