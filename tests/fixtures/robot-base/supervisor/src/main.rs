#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Phoxal supports Linux and macOS only");

use phoxal::artifact::application::{encode_application_contract, ApplicationContract, APPLICATION_RECORD_BYTES, BUNDLE_CONTRACT, EXECUTION_PROTOCOL_CONTRACT, SIMULATION_PROTOCOL_CONTRACT, SUPERVISOR_LAUNCH_CONTRACT, HOST_EXECUTION_TARGET};
const APPLICATION_CONTRACT: ApplicationContract = ApplicationContract { bundle: Some(BUNDLE_CONTRACT), launch: SUPERVISOR_LAUNCH_CONTRACT, execution: Some(EXECUTION_PROTOCOL_CONTRACT), simulation: Some(SIMULATION_PROTOCOL_CONTRACT), target: HOST_EXECUTION_TARGET };
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__phoxal_app"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".phoxal_app"))]
static EMBEDDED_APPLICATION_CONTRACT: [u8; APPLICATION_RECORD_BYTES] = encode_application_contract(&APPLICATION_CONTRACT);
fn main() { std::hint::black_box(&EMBEDDED_APPLICATION_CONTRACT); println!("phoxal-supervisor 0.0.0-dev.8"); }
