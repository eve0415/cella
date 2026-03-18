use clap::Args;

/// Check system dependencies and configuration.
#[derive(Args)]
pub struct DoctorArgs;

impl DoctorArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella doctor: not yet implemented");
        Err("not yet implemented".into())
    }
}
