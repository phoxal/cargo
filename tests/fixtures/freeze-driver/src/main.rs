//! Inert, compiled standard participant for command-snapshot qualification.
//! The fake native probe measures the staged products; this fixture is never
//! used as evidence of hardware or simulated driver behavior.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Phoxal supports Linux and macOS only");

phoxal::api!();
use phoxal::runtime::Context;

#[derive(serde::Serialize, serde::Deserialize, phoxal::Config)]
struct Config { id: u8 }

// The component declaration owns the standard motor and encoder endpoints.
#[phoxal::endpoints]
struct DriverApi {}
struct Driver;
#[phoxal::runtime(contract = DriverApi, period_ms = 20)]
impl Driver {
    #[init]
    fn new(_config: Config) -> phoxal::Result<Self> { Ok(Self) }
    #[step]
    fn advance(&mut self, _ctx: &mut Context<'_, Self>) -> phoxal::Result<()> { Ok(()) }
}
fn main() -> phoxal::Result<()> { phoxal::runtime::run::<Driver>() }
