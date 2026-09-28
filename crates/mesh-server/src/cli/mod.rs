pub mod doctor;
pub mod graph;
pub mod hooks;
pub mod init;
pub mod stats;

pub use doctor::DoctorCommand;
pub use graph::GraphCommand;
pub use hooks::HooksCommand;
pub use init::InitCommand;
pub use stats::StatsCommand;

/// `docs/agent-setup.md`, embedded so `mesh-mcp agent-guide` always prints the
/// guide matching this binary, offline, whatever way it was installed.
pub const AGENT_SETUP_GUIDE: &str = include_str!("../../../../docs/agent-setup.md");
