use std::env;
use std::path::PathBuf;

use draftline::{DiagnosticCode, DraftlineError, Workspace};
use serde::Serialize;

enum CliFailure {
    Usage(String),
    Draftline(DraftlineError),
}

impl std::fmt::Display for CliFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliFailure::Usage(message) => formatter.write_str(message),
            CliFailure::Draftline(error) => error.fmt(formatter),
        }
    }
}

impl From<DraftlineError> for CliFailure {
    fn from(error: DraftlineError) -> Self {
        CliFailure::Draftline(error)
    }
}

impl From<serde_json::Error> for CliFailure {
    fn from(error: serde_json::Error) -> Self {
        CliFailure::Draftline(error.into())
    }
}

#[derive(Serialize)]
struct CliError {
    code: &'static str,
    message: String,
}

fn main() {
    if let Err(error) = run() {
        let cli_error = CliError {
            code: match &error {
                CliFailure::Usage(_) => "invalid_arguments",
                CliFailure::Draftline(_) => "command_failed",
            },
            message: error.to_string(),
        };
        eprintln!(
            "{}",
            serde_json::to_string(&cli_error).unwrap_or_else(|_| "{}".to_string())
        );
        std::process::exit(1);
    }
}

fn run() -> Result<(), CliFailure> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") || args.is_empty() {
        print_help();
        return Ok(());
    }

    let command = args.remove(0);
    match command.as_str() {
        "inspect" => {
            require_json(&mut args)?;
            let workspace = Workspace::open(workspace_path(args)?)?;
            println!("{}", workspace.inspect_json()?);
        }
        "capabilities" => {
            require_json(&mut args)?;
            require_no_args(&args)?;
            println!("{}", Workspace::capabilities_json()?);
        }
        "verify" => {
            require_json(&mut args)?;
            let workspace = Workspace::open(workspace_path(args)?)?;
            println!("{}", serde_json::to_string(&workspace.verify_workspace()?)?);
        }
        "explain-error" => {
            require_json(&mut args)?;
            let [code] = args.as_slice() else {
                return Err(CliFailure::Usage(
                    "expected exactly one diagnostic code".to_string(),
                ));
            };
            println!(
                "{}",
                serde_json::to_string(&Workspace::explain_error(parse_diagnostic_code(code)?))?
            );
        }
        _ => {
            return Err(CliFailure::Usage(format!("unknown command: {command}")));
        }
    }

    Ok(())
}

fn require_no_args(args: &[String]) -> Result<(), CliFailure> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(CliFailure::Usage("unexpected arguments".to_string()))
    }
}

fn require_json(args: &mut Vec<String>) -> Result<(), CliFailure> {
    if let Some(index) = args.iter().position(|arg| arg == "--json") {
        args.remove(index);
        Ok(())
    } else {
        Err(CliFailure::Usage("--json is required".to_string()))
    }
}

fn workspace_path(args: Vec<String>) -> Result<PathBuf, CliFailure> {
    match args.as_slice() {
        [] => env::current_dir().map_err(|error| CliFailure::Draftline(error.into())),
        [path] => Ok(PathBuf::from(path)),
        _ => Err(CliFailure::Usage(
            "expected at most one workspace path".to_string(),
        )),
    }
}

fn parse_diagnostic_code(code: &str) -> Result<DiagnosticCode, CliFailure> {
    match code {
        "recovery_required" => Ok(DiagnosticCode::RecoveryRequired),
        "workspace_locked" => Ok(DiagnosticCode::WorkspaceLocked),
        "dirty_workspace" => Ok(DiagnosticCode::DirtyWorkspace),
        "local_only_workspace" => Ok(DiagnosticCode::LocalOnlyWorkspace),
        "shared_capable_workspace" => Ok(DiagnosticCode::SharedCapableWorkspace),
        "no_current_variation" => Ok(DiagnosticCode::NoCurrentVariation),
        "workspace_read_failed" => Ok(DiagnosticCode::WorkspaceReadFailed),
        "policy_tracked_file_ignored" => Ok(DiagnosticCode::PolicyTrackedFileIgnored),
        _ => Err(CliFailure::Usage(format!(
            "unknown diagnostic code: {code}"
        ))),
    }
}

fn print_help() {
    println!(
        "draftline inspect --json [path]\n\
         draftline capabilities --json\n\
         draftline verify --json [path]\n\
         draftline explain-error --json <diagnostic_code>"
    );
}
