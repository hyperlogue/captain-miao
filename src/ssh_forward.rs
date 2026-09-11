//! SSH forwarding configuration, shared by the editor and the connection task.
//!
//! Keep the OpenSSH spec verbatim on disk: socket forwards, IPv6 and future SSH
//! syntax must survive opening the form. The common TCP shape has a structured
//! editor; everything else uses a raw spec. No shell evaluates these arguments.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Forward {
    pub flag: String,
    pub spec: String,
}

impl std::fmt::Display for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.flag, shell_words::quote(&self.spec))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rule {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(flatten)]
    pub forward: Forward,
}

impl From<Forward> for Rule {
    fn from(forward: Forward) -> Self {
        Self {
            name: String::new(),
            disabled: false,
            forward,
        }
    }
}

/// Extract only standalone/glued forwarding switches. Consume other switches'
/// operands first: an IdentityFile named `-Local` is not a forwarding rule.
/// Incomplete flags stay visible in the advanced editor instead of vanishing.
pub(crate) fn split_options(args: &[String]) -> (Vec<String>, Vec<Forward>) {
    let mut options = Vec::new();
    let mut forwards = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if matches!(arg.as_str(), "-L" | "-R" | "-D") {
            if let Some(spec) = rest.next() {
                forwards.push(Forward {
                    flag: arg.clone(),
                    spec: spec.clone(),
                });
            } else {
                options.push(arg.clone());
            }
        } else if arg.len() > 2 && matches!(arg.get(..2), Some("-L" | "-R" | "-D")) {
            forwards.push(Forward {
                flag: arg[..2].into(),
                spec: arg[2..].into(),
            });
        } else {
            options.push(arg.clone());
            if matches!(
                arg.as_str(),
                "-B" | "-b"
                    | "-c"
                    | "-E"
                    | "-e"
                    | "-F"
                    | "-I"
                    | "-i"
                    | "-J"
                    | "-l"
                    | "-m"
                    | "-O"
                    | "-o"
                    | "-p"
                    | "-Q"
                    | "-S"
                    | "-W"
                    | "-w"
            ) && let Some(value) = rest.next()
            {
                options.push(value.clone());
            }
        }
    }
    (options, forwards)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TcpForward {
    pub bind: String,
    pub port: String,
    pub destination: String,
    pub destination_port: String,
}

fn parts(spec: &str) -> Option<Vec<&str>> {
    let mut bracket = false;
    let mut start = 0;
    let mut parts = Vec::new();
    for (i, c) in spec.char_indices() {
        match c {
            '[' if !bracket => bracket = true,
            ']' if bracket => bracket = false,
            ':' if !bracket => {
                parts.push(&spec[start..i]);
                start = i + 1;
            }
            '[' | ']' => return None,
            _ => {}
        }
    }
    if bracket {
        return None;
    }
    parts.push(&spec[start..]);
    Some(parts)
}

fn port(text: &str) -> bool {
    text.parse::<u16>().is_ok_and(|p| p != 0)
}

fn address(text: &str) -> bool {
    !text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '\0' | '/' | '[' | ']'))
}

fn bracket(text: &str) -> String {
    if text.contains(':') {
        format!("[{text}]")
    } else {
        text.to_owned()
    }
}

impl Forward {
    pub fn tcp(&self) -> Option<TcpForward> {
        let p = parts(&self.spec)?;
        let (bind, listen, dest, dest_port) = match (self.flag.as_str(), p.as_slice()) {
            ("-D", [p]) => ("localhost", *p, "", ""),
            ("-D", [b, p]) => (*b, *p, "", ""),
            ("-L" | "-R", [p, d, dp]) => ("localhost", *p, *d, *dp),
            ("-L" | "-R", [b, p, d, dp]) => (*b, *p, *d, *dp),
            _ => return None,
        };
        if !port(listen) || (self.flag != "-D" && !port(dest_port)) {
            return None;
        }
        Some(TcpForward {
            bind: bind.trim_matches(['[', ']']).into(),
            port: listen.into(),
            destination: dest.trim_matches(['[', ']']).into(),
            destination_port: dest_port.into(),
        })
    }

    pub fn from_tcp(flag: &str, tcp: &TcpForward) -> Result<Self, String> {
        if !port(&tcp.port) {
            return Err("Listen port must be between 1 and 65535".into());
        }
        if !address(&tcp.bind) {
            return Err(
                "Enter a bind address (localhost for loopback, * for all interfaces)".into(),
            );
        }
        let spec = if flag == "-D" {
            format!("{}:{}", bracket(&tcp.bind), tcp.port)
        } else {
            if !port(&tcp.destination_port) {
                return Err("Destination port must be between 1 and 65535".into());
            }
            if !address(&tcp.destination) {
                return Err("Enter a destination host or IP address".into());
            }
            format!(
                "{}:{}:{}:{}",
                bracket(&tcp.bind),
                tcp.port,
                bracket(&tcp.destination),
                tcp.destination_port
            )
        };
        let forward = Self {
            flag: flag.into(),
            spec,
        };
        forward.validate()?;
        Ok(forward)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.flag.as_str(), "-L" | "-R" | "-D") {
            return Err("Choose local (-L), remote (-R), or SOCKS (-D) forwarding".into());
        }
        if self.spec.is_empty() || self.spec.chars().any(char::is_control) {
            return Err("Enter a nonempty SSH forwarding specification".into());
        }
        // Remote port zero is supported by SSH, but its allocated port must be
        // used to cancel it. Until we persist that acknowledgement, reject it.
        if self.flag == "-R"
            && let Some(p) = parts(&self.spec)
        {
            let listen = match p.as_slice() {
                [p] | [p, _, _] => *p,
                [_, p] | [_, p, _, _] => *p,
                _ => "",
            };
            if listen == "0" {
                return Err(
                    "Choose a fixed remote listen port; automatic port allocation is not supported"
                        .into(),
                );
            }
        }
        Ok(())
    }

    pub fn description(&self) -> String {
        let Some(t) = self.tcp() else {
            return self.to_string();
        };
        let listen = format!("{}:{}", bracket(&t.bind), t.port);
        let dest = format!("{}:{}", bracket(&t.destination), t.destination_port);
        match self.flag.as_str() {
            "-R" => format!("Remote {listen} → This machine {dest}"),
            "-D" => format!("This machine {listen} → SOCKS via remote"),
            _ => format!("This machine {listen} → Remote {dest}"),
        }
    }

    pub fn conflicts(&self, other: &Self) -> bool {
        if (self.flag == "-R") != (other.flag == "-R") {
            return false;
        }
        if self == other {
            return true;
        }
        match (self.tcp(), other.tcp()) {
            (Some(a), Some(b)) => {
                a.port == b.port
                    && (a.bind == b.bind
                        || ["", "*", "0.0.0.0", "::"].contains(&a.bind.as_str())
                        || ["", "*", "0.0.0.0", "::"].contains(&b.bind.as_str())
                        || (a.bind == "localhost"
                            && ["127.0.0.1", "::1"].contains(&b.bind.as_str()))
                        || (b.bind == "localhost"
                            && ["127.0.0.1", "::1"].contains(&a.bind.as_str())))
            }
            _ => false,
        }
    }
}

pub(crate) fn validate_rules(rules: &[Rule]) -> Result<(), String> {
    for (i, rule) in rules.iter().enumerate() {
        if !rule.disabled {
            rule.forward.validate()?;
        }
        if !rule.disabled
            && rules[..i]
                .iter()
                .any(|r| !r.disabled && r.forward.conflicts(&rule.forward))
        {
            return Err(format!(
                "Another enabled rule already uses this listener: {}",
                rule.forward
            ));
        }
    }
    Ok(())
}

/// Bulk input is either a list of ports or SSH forwarding switches. Fail the
/// whole import if any token is unknown; a partial import silently loses work.
pub(crate) fn import(text: &str) -> Result<Vec<Rule>, String> {
    let args = shell_words::split(text).map_err(|e| e.to_string())?;
    if args.is_empty() {
        return Err("Enter ports or -L/-R/-D arguments".into());
    }
    let rules = if args.iter().all(|a| port(a)) {
        args.into_iter()
            .map(|p| {
                Rule::from(Forward {
                    flag: "-L".into(),
                    spec: format!("localhost:{p}:localhost:{p}"),
                })
            })
            .collect()
    } else {
        let (rest, forwards) = split_options(&args);
        if !rest.is_empty() {
            return Err(format!(
                "Not a forwarding argument: {}",
                shell_words::join(rest)
            ));
        }
        forwards.into_iter().map(Rule::from).collect::<Vec<_>>()
    };
    validate_rules(&rules)?;
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_handles_ports_ipv6_and_socket_specs_without_losing_arguments() {
        let rules = import("3000 8080").unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1].forward.tcp().unwrap().destination_port, "8080");
        let raw =
            "-L '[::1]:8080:[::1]:3000' -R '/path/to/remote socket:/path/to/local socket' -D1080";
        let rules = import(raw).unwrap();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].forward.tcp().unwrap().destination, "::1");
        assert!(rules[1].forward.tcp().is_none());
        assert_eq!(
            import(
                &rules
                    .iter()
                    .map(|r| r.forward.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            )
            .unwrap(),
            rules
        );
        assert!(import("3000 garbage").is_err());
        assert!(import("-L").is_err());
        let args =
            shell_words::split("-i -Local -o 'ProxyCommand=helper -L' -L3000:localhost:3000")
                .unwrap();
        let (options, forwards) = split_options(&args);
        assert_eq!(options, ["-i", "-Local", "-o", "ProxyCommand=helper -L"]);
        assert_eq!(forwards.len(), 1);
    }

    #[test]
    fn conflicting_listeners_and_unmanageable_allocated_ports_are_rejected() {
        assert!(import("-L 3000:localhost:3000 -D3000").is_err());
        assert!(import("-L '*:3000:localhost:3000' -L 127.0.0.1:3000:localhost:8080").is_err());
        assert!(import("-R 0:localhost:3000").is_err());
        assert!(import("-L3000:localhost:3000 -R3000:localhost:3000").is_ok());
    }
}
