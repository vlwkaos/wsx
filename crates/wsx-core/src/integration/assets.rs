use super::{IntegrationTarget, LifecycleCapability};

const SHELL: &str = include_str!("../../integrations/common/wsx-agent-status.sh");
const PLUGIN: &str = include_str!("../../integrations/common/wsx-opencode-agent-status.js");
const PI: &str = include_str!("../../integrations/pi/wsx-agent-status.ts");
const OMP: &str = include_str!("../../integrations/omp/wsx-agent-status.ts");
pub(crate) const OPENCODE_TUI: &str =
    include_str!("../../integrations/opencode/wsx-tui-session.js");
pub(crate) const REPORTER: &str = include_str!("../../integrations/common/wsx-reporter.mjs");
pub(crate) const PYTHON_REPORTER: &str = include_str!("../../integrations/common/wsx-reporter.py");
const HERMES: &str = include_str!("../../integrations/hermes/__init__.py");
pub(crate) const HERMES_MANIFEST: &str = include_str!("../../integrations/hermes/plugin.yaml");

/// The Claude output style: the presentation rule, with Claude Code's own coding instructions kept.
/// ^ https://code.claude.com/docs/en/output-styles
pub(crate) const OUTPUT_STYLE_NAME: &str = "wsx";
pub(crate) fn output_style() -> String {
    format!(
        "---\nname: {OUTPUT_STYLE_NAME}\ndescription: One screen: the answer first, structure as a diagram or a table, each fact once\nkeep-coding-instructions: true\n---\n\n{}",
        super::PRESENTATION
    )
}

pub(crate) fn reporter(target: IntegrationTarget) -> (&'static str, &'static str) {
    match target {
        IntegrationTarget::Pi
        | IntegrationTarget::Omp
        | IntegrationTarget::Opencode
        | IntegrationTarget::Kilo => ("wsx-reporter.mjs", REPORTER),
        _ => ("wsx-reporter.py", PYTHON_REPORTER),
    }
}

pub(crate) fn primary(target: IntegrationTarget) -> String {
    match target {
        IntegrationTarget::Pi => PI.replace("../common/wsx-reporter.mjs", "./wsx-reporter.mjs"),
        IntegrationTarget::Omp => OMP.replace("../common/wsx-reporter.mjs", "./wsx-reporter.mjs"),
        IntegrationTarget::Opencode | IntegrationTarget::Kilo => PLUGIN
            .replace("@VERSION@", &target.expected_version().to_string())
            .replace("@PROVIDER@", target.cli_value()),
        IntegrationTarget::Hermes => {
            HERMES.replace("../common/wsx-reporter.py", "./wsx-reporter.py")
        }
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
