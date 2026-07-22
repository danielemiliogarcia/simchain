//! Operating-system boundary for P2P interface discovery and impairment.

use simchain_common::internal_api::NetworkImpairment;
use std::io::Write;
use std::process::{Command, Stdio};

pub trait NetworkSystem: Send + Sync {
    fn detect_p2p_interface(&self) -> anyhow::Result<String>;
    fn apply(&self, interface: &str, impairment: &NetworkImpairment) -> anyhow::Result<()>;
    fn clear(&self, interface: &str) -> anyhow::Result<()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandSpec {
    program: &'static str,
    args: Vec<String>,
    stdin: Option<String>,
    acceptable_failure_text: Vec<&'static str>,
}

pub struct CommandNetworkSystem {
    probe_ip: String,
}

impl CommandNetworkSystem {
    pub fn new(probe_ip: String) -> Self {
        Self { probe_ip }
    }

    fn run(spec: CommandSpec) -> anyhow::Result<String> {
        let mut command = Command::new(spec.program);
        command.args(&spec.args);
        if spec.stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| anyhow::anyhow!("start {}: {error}", spec.program))?;
        if let Some(stdin) = spec.stdin {
            child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("{} stdin was unavailable", spec.program))?
                .write_all(stdin.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let acceptable_failure = spec
            .acceptable_failure_text
            .iter()
            .any(|text| stderr.contains(text));
        if !output.status.success() && !acceptable_failure {
            anyhow::bail!(
                "{} {} failed: {}",
                spec.program,
                spec.args.join(" "),
                stderr.trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

impl NetworkSystem for CommandNetworkSystem {
    fn detect_p2p_interface(&self) -> anyhow::Result<String> {
        let output = Self::run(route_spec(&self.probe_ip))?;
        parse_route_interface(&output)
    }

    fn apply(&self, interface: &str, impairment: &NetworkImpairment) -> anyhow::Result<()> {
        validate_interface(interface)?;
        self.clear(interface)?;
        let spec = match impairment {
            NetworkImpairment::Netem { delay_ms, loss_pct } => {
                netem_spec(interface, *delay_ms, *loss_pct)
            }
            NetworkImpairment::Partition {
                ingress_drop,
                egress_drop,
            } => partition_spec(interface, *ingress_drop, *egress_drop),
        };
        if let Err(error) = Self::run(spec) {
            let cleanup = self.clear(interface).err();
            return match cleanup {
                Some(cleanup) => Err(error.context(format!(
                    "rollback after impairment failure also failed: {cleanup}"
                ))),
                None => Err(error),
            };
        }
        Ok(())
    }

    fn clear(&self, interface: &str) -> anyhow::Result<()> {
        validate_interface(interface)?;
        Self::run(clear_nft_spec())?;
        Self::run(clear_netem_spec(interface))?;
        Ok(())
    }
}

fn route_spec(probe_ip: &str) -> CommandSpec {
    CommandSpec {
        program: "ip",
        args: vec!["-o".into(), "route".into(), "get".into(), probe_ip.into()],
        stdin: None,
        acceptable_failure_text: Vec::new(),
    }
}

fn netem_spec(interface: &str, delay_ms: u64, loss_pct: f64) -> CommandSpec {
    CommandSpec {
        program: "tc",
        args: vec![
            "qdisc".into(),
            "replace".into(),
            "dev".into(),
            interface.into(),
            "root".into(),
            "netem".into(),
            "delay".into(),
            format!("{delay_ms}ms"),
            "loss".into(),
            format!("{loss_pct}%"),
        ],
        stdin: None,
        acceptable_failure_text: Vec::new(),
    }
}

fn partition_spec(interface: &str, ingress_drop: bool, egress_drop: bool) -> CommandSpec {
    let mut rules = String::from("table inet simchain_agent {\n");
    if ingress_drop {
        rules.push_str(&format!(
            " chain input {{ type filter hook input priority -10; policy accept; iifname \"{interface}\" drop; }}\n"
        ));
    }
    if egress_drop {
        rules.push_str(&format!(
            " chain output {{ type filter hook output priority -10; policy accept; oifname \"{interface}\" drop; }}\n"
        ));
    }
    rules.push_str("}\n");
    CommandSpec {
        program: "nft",
        args: vec!["-f".into(), "-".into()],
        stdin: Some(rules),
        acceptable_failure_text: Vec::new(),
    }
}

fn clear_nft_spec() -> CommandSpec {
    CommandSpec {
        program: "nft",
        args: vec![
            "delete".into(),
            "table".into(),
            "inet".into(),
            "simchain_agent".into(),
        ],
        stdin: None,
        acceptable_failure_text: vec!["No such file or directory"],
    }
}

fn clear_netem_spec(interface: &str) -> CommandSpec {
    CommandSpec {
        program: "tc",
        args: vec![
            "qdisc".into(),
            "del".into(),
            "dev".into(),
            interface.into(),
            "root".into(),
        ],
        stdin: None,
        acceptable_failure_text: vec![
            "No such file or directory",
            "Cannot delete qdisc with handle of zero",
        ],
    }
}

fn parse_route_interface(output: &str) -> anyhow::Result<String> {
    let fields: Vec<&str> = output.split_whitespace().collect();
    let interface = fields
        .windows(2)
        .find_map(|window| (window[0] == "dev").then_some(window[1]))
        .ok_or_else(|| anyhow::anyhow!("route output did not contain a dev field"))?;
    validate_interface(interface)?;
    anyhow::ensure!(interface != "lo", "P2P route unexpectedly targets loopback");
    Ok(interface.to_string())
}

fn validate_interface(interface: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !interface.is_empty()
            && interface.len() <= 15
            && interface
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
        "invalid network interface name"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_parser_targets_the_declared_device() {
        assert_eq!(
            parse_route_interface("172.30.0.254 dev eth1 src 172.30.0.2 uid 0\n")
                .expect("interface"),
            "eth1"
        );
        assert!(parse_route_interface("unreachable 172.30.0.254").is_err());
        assert!(parse_route_interface("local 127.0.0.1 dev lo").is_err());
    }

    #[test]
    fn netem_command_is_argument_safe_and_p2p_scoped() {
        let spec = netem_spec("eth1", 250, 1.5);
        assert_eq!(spec.program, "tc");
        assert_eq!(
            spec.args,
            [
                "qdisc", "replace", "dev", "eth1", "root", "netem", "delay", "250ms", "loss",
                "1.5%"
            ]
        );
        assert!(spec.stdin.is_none());
    }

    #[test]
    fn partition_rules_are_symmetric_and_interface_scoped() {
        let spec = partition_spec("eth1", true, true);
        let rules = spec.stdin.expect("rules");
        assert!(rules.contains("iifname \"eth1\" drop"));
        assert!(rules.contains("oifname \"eth1\" drop"));
        assert!(!rules.contains("eth0"));
    }

    #[test]
    fn interface_validation_rejects_injection_text() {
        assert!(validate_interface("eth1;reboot").is_err());
        assert!(validate_interface("").is_err());
    }

    #[test]
    fn clear_only_tolerates_already_absent_state() {
        assert_eq!(
            clear_nft_spec().acceptable_failure_text,
            ["No such file or directory"]
        );
        let tc = clear_netem_spec("eth1");
        assert!(tc
            .acceptable_failure_text
            .contains(&"No such file or directory"));
        assert!(!tc
            .acceptable_failure_text
            .iter()
            .any(|message| message.contains("permitted")));
    }
}
