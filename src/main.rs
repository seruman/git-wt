use clap::Parser;
use std::io::{self, Write};

fn main() {
    let cli = git_wt::Cli::parse();

    match git_wt::execute(cli) {
        Ok(output) => {
            let mut stdout = io::stdout();
            if let Err(error) = stdout
                .write_all(output.as_bytes())
                .and_then(|()| stdout.flush())
                && error.kind() != io::ErrorKind::BrokenPipe
            {
                let error = anyhow::Error::new(error).context("writing stdout");
                let _ = writeln!(io::stderr(), "git-wt: {error:#}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            let _ = writeln!(io::stderr(), "git-wt: {error:#}");
            std::process::exit(if error.is::<git_wt::UsageError>() {
                2
            } else {
                1
            });
        }
    }
}
