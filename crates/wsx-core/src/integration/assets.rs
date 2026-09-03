use super::{IntegrationTarget, LifecycleCapability};

const SHELL: &str = include_str!("../../integrations/common/wsx-agent-status.sh");
const PLUGIN: &str = include_str!("../../integrations/common/wsx-opencode-agent-status.js");
const PI: &str = include_str!("../../integrations/pi/wsx-agent-status.ts");
const OMP: &str = include_str!("../../integrations/omp/wsx-agent-status.ts");
pub(crate) const OPENCODE_TUI: &str =
    include_str!("../../integrations/opencode/wsx-tui-session.js");
const HERMES: &str = include_str!("../../integrations/hermes/__init__.py");
pub(crate) const HERMES_MANIFEST: &str = include_str!("../../integrations/hermes/plugin.yaml");

pub(crate) fn primary(target: IntegrationTarget) -> String {
    match target {
        IntegrationTarget::Pi => PI.to_string(),
        IntegrationTarget::Omp => OMP.to_string(),
        IntegrationTarget::Opencode | IntegrationTarget::Kilo => PLUGIN
            .replace("@VERSION@", &target.expected_version().to_string())
            .replace("@PROVIDER@", target.cli_value()),
        IntegrationTarget::Hermes => HERMES.to_string(),
        _ => SHELL
            .replace("@VERSION@", &target.expected_version().to_string())
            .replace("@PROVIDER@", target.cli_value())
            .replace(
                "@LIFECYCLE@",
                if target.lifecycle() == LifecycleCapability::Authoritative {
                    "yes"
                } else {
                    "no"
                },
            ),
    }
}
