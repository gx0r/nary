use crate::error::Result;
use crate::SandboxProfileArgs;

pub fn run_sandbox_profile(args: &SandboxProfileArgs) -> Result<()> {
    let project_root = match &args.project {
        Some(p) => std::fs::canonicalize(p)?,
        None => std::env::current_dir()?,
    };
    let profile = nary_lib::generate_sandbox_profile(&project_root);
    print!("{}", profile);
    Ok(())
}
