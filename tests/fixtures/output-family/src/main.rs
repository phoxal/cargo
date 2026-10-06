phoxal::api!();
use phoxal::contracts::component::actuator::ActuatorCommand;

#[phoxal::endpoints]
struct ProviderApi {
    #[phoxal::output(projection = state, lease_ms = 100, max_bytes = 64,
        family = "/wheels", suffix = "_actuator", max_ports = 8)]
    wheels: phoxal::contracts::Latest<ActuatorCommand>,
}

#[derive(serde::Deserialize, phoxal::Config)]
struct Config {
    wheels: std::collections::BTreeMap<String, ()>,
}

struct Provider {
    names: Vec<String>,
}

#[phoxal::runtime(contract = ProviderApi, period_ms = 20)]
impl Provider {
    #[init]
    fn initialize(config: Config) -> phoxal::Result<Self> {
        Ok(Self { names: config.wheels.into_keys().collect() })
    }

    #[publish(wheels)]
    fn wheels(&self) -> Vec<(String, Option<phoxal::contracts::component::actuator::ActuatorCommand>)> {
        self.names.iter().map(|name| (name.clone(), Some(phoxal::contracts::component::actuator::ActuatorCommand {
            control: Some(phoxal::contracts::component::actuator::Control::VelocityRadps(0.0)),
        }))).collect()
    }
}

fn main() -> phoxal::Result<()> {
    phoxal::runtime::run::<Provider>()
}
