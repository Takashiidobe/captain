use std::process::ExitCode;

fn main() -> ExitCode {
    match captain::parse_args(std::env::args()).and_then(|config| captain::check(&config)) {
        Ok(report) => {
            println!("{}", captain::format_report(&report));
            if report.is_compatible() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(captain::Error::Usage(message)) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}
