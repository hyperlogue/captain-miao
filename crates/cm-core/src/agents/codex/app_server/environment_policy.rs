//! Resolve the daemon's policy before writing any direnv values to disk.
use super::environment::{Delta, reserved, variable_name};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Policy {
    inherit: Option<String>,
    ignore_default_excludes: Option<bool>,
    exclude: Vec<String>,
    include_only: Vec<String>,
    filters: BTreeMap<String, String>,
    set: BTreeMap<String, String>,
    experimental_use_profile: Option<bool>,
}

impl Policy {
    pub(super) fn resolve(base: &Value, overrides: &Value) -> Result<(Self, Value)> {
        let mut config = base.clone();
        ensure!(config.is_object(), "invalid Codex effective configuration");
        if let Some(overrides) = overrides.as_object() {
            // Match Codex's TOML dotted-key spelling, including quoted patterns.
            for (key, value) in overrides {
                let keys = toml_edit::Key::parse(key)
                    .map_err(|_| anyhow::anyhow!("invalid Codex configuration key"))?;
                let mut patch = value.clone();
                for key in keys.into_iter().rev() {
                    patch = json!({key.get():patch});
                }
                merge(&mut config, patch);
            }
        } else {
            ensure!(overrides.is_null(), "invalid Codex thread configuration");
        }
        let mut value = config["shell_environment_policy"].clone();
        if value.is_null() {
            value = json!({});
        }
        value
            .as_object_mut()
            .context("invalid Codex environment policy")?
            .retain(|_, v| !v.is_null());
        let policy: Self = serde_json::from_value(value.clone())
            .map_err(|_| anyhow::anyhow!("unsupported Codex shell environment policy"))?;
        ensure!(
            policy
                .inherit
                .as_deref()
                .is_none_or(|v| matches!(v, "all" | "core" | "none")),
            "unsupported Codex environment inheritance"
        );
        ensure!(
            policy.experimental_use_profile != Some(true),
            "direnv cannot preserve experimental_use_profile=true"
        );
        ensure!(
            policy
                .filters
                .values()
                .all(|v| matches!(v.as_str(), "include" | "exclude")),
            "unsupported Codex environment filter"
        );
        // config/read may normalize legacy arrays to filters. Mixed forms in
        // separate layers are combined conservatively, without dropping denies.
        Ok((policy, value))
    }

    fn included(&self, key: &str) -> bool {
        let includes: Vec<_> = self
            .include_only
            .iter()
            .chain(
                self.filters
                    .iter()
                    .filter(|(_, action)| *action == "include")
                    .map(|(pattern, _)| pattern),
            )
            .collect();
        includes.is_empty() || includes.iter().any(|pattern| matches(pattern, key))
    }

    fn inherited(&self, key: &str) -> bool {
        match self.inherit.as_deref().unwrap_or("all") {
            "none" => false,
            "core" => [
                "PATH", "SHELL", "TMPDIR", "TEMP", "TMP", "HOME", "LANG", "LC_ALL", "LC_CTYPE",
                "LOGNAME", "USER",
            ]
            .iter()
            .any(|core| key.eq_ignore_ascii_case(core)),
            _ => true,
        }
    }

    fn excluded(&self, key: &str) -> bool {
        (self.ignore_default_excludes == Some(false)
            && ["*KEY*", "*SECRET*", "*TOKEN*"]
                .iter()
                .any(|pattern| matches(pattern, key)))
            || self.exclude.iter().any(|pattern| matches(pattern, key))
            || self
                .filters
                .iter()
                .any(|(pattern, action)| action == "exclude" && matches(pattern, key))
    }

    pub(super) fn apply(&self, delta: &Delta) -> Delta {
        let mut filtered = delta.clone();
        for (key, value) in &mut filtered {
            if !self.included(key) || self.excluded(key) || !self.inherited(key) {
                *value = None;
            }
        }
        // Explicit Codex overrides have their ordinary precedence over direnv,
        // including an intentional restoration after exclusion. The allowlist
        // still applies, just as it does in Codex itself.
        for (key, value) in &self.set {
            if variable_name(key) && !reserved(key) {
                filtered.insert(key.clone(), self.included(key).then(|| value.clone()));
            }
        }
        filtered
    }

    /// Bash's manual-command login profiles run after Codex filters the
    /// environment. Reapply restrictions to exported names before the delta,
    /// so a profile cannot inadvertently restore an excluded credential.
    pub(super) fn scrub_script(&self) -> String {
        let mut script = String::from(
            "for _CM_DIRENV_NAME in $(if [ -n \"${BASH_VERSION-}\" ]; then export -p | while IFS= read -r _CM_DIRENV_LINE; do case \"$_CM_DIRENV_LINE\" in 'declare -x '*) _CM_DIRENV_LINE=${_CM_DIRENV_LINE#declare -x }; _CM_DIRENV_LINE=${_CM_DIRENV_LINE%%=*}; case \"$_CM_DIRENV_LINE\" in ''|[0-9]*|*[!a-zA-Z0-9_]*) continue;; esac; printf '%s\\n' \"$_CM_DIRENV_LINE\";; esac; done; else print -rl -- ${(k)parameters[(R)*export*]}; fi); do\n",
        );
        script.push_str("case \"$_CM_DIRENV_NAME\" in CODEX_*|BASH_ENV|ZDOTDIR|SHLVL|PWD|OLDPWD|_|SHELLOPTS|BASHOPTS|UID|EUID|PPID|_CM_DIRENV_*) continue;; esac\n");
        shell_case(
            &mut script,
            &[
                "DIRENV_*",
                "NODE_REPL_AUTH_TOKEN",
                "OPENAI_FEDERATION_RULE_ID",
                "OPENAI_IDENTITY_TOKEN_FILE",
                "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
            ]
            .map(str::to_owned),
            "unset \"$_CM_DIRENV_NAME\"; continue",
            false,
        );
        // Preserve the proxy environment Codex adds after native filtering.
        script.push_str(
            "if [ -n \"${CODEX_NETWORK_PROXY_CREDENTIAL_BROKER_ACTIVE+x}\" ]; then continue; fi\n",
        );
        script.push_str("if [ -n \"${CODEX_NETWORK_PROXY_ACTIVE+x}\" ]; then\ncase \"$_CM_DIRENV_NAME\" in *[pP][rR][oO][xX][yY]*|GIT_SSH_COMMAND|SSL_CERT_FILE|SSL_CERT_DIR|REQUESTS_CA_BUNDLE|CURL_CA_BUNDLE|NODE_EXTRA_CA_CERTS|GIT_SSL_CAINFO|CARGO_HTTP_CAINFO|PIP_CERT|BUNDLE_SSL_CA_CERT|npm_config_cafile|NPM_CONFIG_CAFILE) continue;; esac\nfi\n");
        // Explicit set follows exclusions but still obeys the allowlist.
        let sets: Vec<_> = self
            .set
            .keys()
            .filter(|k| self.included(k) && variable_name(k) && !reserved(k))
            .cloned()
            .collect();
        if !sets.is_empty() {
            let names = sets
                .iter()
                .map(|name| crate::agents::shell_quote(name))
                .collect::<Vec<_>>()
                .join("|");
            script.push_str(&format!(
                "case \"$_CM_DIRENV_NAME\" in {names}) continue;; esac\n"
            ));
        }
        let includes: Vec<_> = self
            .include_only
            .iter()
            .chain(
                self.filters
                    .iter()
                    .filter(|(_, v)| *v == "include")
                    .map(|(k, _)| k),
            )
            .cloned()
            .collect();
        shell_case(
            &mut script,
            &includes,
            "unset \"$_CM_DIRENV_NAME\"; continue",
            true,
        );
        let mut excludes: Vec<_> = self
            .exclude
            .iter()
            .chain(
                self.filters
                    .iter()
                    .filter(|(_, v)| *v == "exclude")
                    .map(|(k, _)| k),
            )
            .cloned()
            .collect();
        if self.ignore_default_excludes == Some(false) {
            excludes.extend(["*KEY*", "*SECRET*", "*TOKEN*"].map(str::to_owned));
        }
        shell_case(
            &mut script,
            &excludes,
            "unset \"$_CM_DIRENV_NAME\"; continue",
            false,
        );
        if self.inherit.as_deref() == Some("none") {
            script.push_str("unset \"$_CM_DIRENV_NAME\"\n");
        } else if self.inherit.as_deref() == Some("core") {
            let core = [
                "PATH", "SHELL", "TMPDIR", "TEMP", "TMP", "HOME", "LANG", "LC_ALL", "LC_CTYPE",
                "LOGNAME", "USER",
            ]
            .map(str::to_owned);
            shell_case(&mut script, &core, "unset \"$_CM_DIRENV_NAME\"", true);
        }
        script.push_str("done\nunset _CM_DIRENV_NAME\n");
        script
    }

    pub(super) fn bootstrap(&self, mut value: Value, hooks: &Value) -> Result<Value> {
        // Preserve the daemon's native inheritance/filtering for everything
        // except the three startup controls. No direnv values cross the RPC.
        let obj = value
            .as_object_mut()
            .context("invalid environment policy")?;
        obj.remove("exclude");
        obj.remove("include_only");
        obj.remove("filters");
        let excludes: Vec<_> = self
            .exclude
            .iter()
            .chain(
                self.filters
                    .iter()
                    .filter(|(_, v)| *v == "exclude")
                    .map(|(k, _)| k),
            )
            .cloned()
            .collect();
        let mut includes: Vec<_> = self
            .include_only
            .iter()
            .chain(
                self.filters
                    .iter()
                    .filter(|(_, v)| *v == "include")
                    .map(|(k, _)| k),
            )
            .cloned()
            .collect();
        if !includes.is_empty() {
            includes.extend(["BASH_ENV", "ZDOTDIR", "SHLVL"].map(str::to_owned));
        }
        obj.insert("exclude".into(), json!(excludes));
        obj.insert("include_only".into(), json!(includes));
        let set = obj
            .entry("set")
            .or_insert(json!({}))
            .as_object_mut()
            .context("invalid Codex environment overrides")?;
        for (key, value) in hooks["set"]
            .as_object()
            .context("invalid startup controls")?
        {
            set.insert(key.clone(), value.clone());
        }
        Ok(value)
    }
}

fn merge(base: &mut Value, overlay: Value) {
    if let (Some(base), Some(overlay)) = (base.as_object_mut(), overlay.as_object()) {
        for (key, value) in overlay {
            merge(base.entry(key).or_insert(Value::Null), value.clone());
        }
    } else {
        *base = overlay;
    }
}

// Codex patterns use only '*' and '?' and compare names case-insensitively.
fn matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let mut reachable = vec![false; name.len() + 1];
    reachable[0] = true;
    for c in pattern.bytes() {
        if c == b'*' {
            for i in 1..reachable.len() {
                reachable[i] |= reachable[i - 1];
            }
        } else {
            for i in (1..reachable.len()).rev() {
                reachable[i] = reachable[i - 1] && (c == b'?' || c == name.as_bytes()[i - 1]);
            }
            reachable[0] = false;
        }
    }
    reachable[name.len()]
}

fn shell_case(script: &mut String, patterns: &[String], command: &str, invert: bool) {
    if patterns.is_empty() {
        return;
    }
    let patterns: Vec<_> = patterns
        .iter()
        .map(|p| {
            p.chars()
                .map(|c| {
                    if c.is_ascii_alphabetic() {
                        format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
                    } else if matches!(c, '*' | '?') {
                        c.to_string()
                    } else {
                        crate::agents::shell_quote(&c.to_string())
                    }
                })
                .collect::<String>()
        })
        .collect();
    let pattern = patterns.join("|");
    script.push_str(&format!(
        "case \"$_CM_DIRENV_NAME\" in {pattern}) {};; {}esac\n",
        if invert { ":" } else { command },
        if invert {
            format!("*) {command};; ")
        } else {
            String::new()
        }
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_apply_to_direnv_before_explicit_overrides_and_allowlists() {
        let (policy, effective) = Policy::resolve(
            &json!({"shell_environment_policy": {
                "inherit":"all", "ignore_default_excludes":false,
                "filters":{"aws_*":"exclude", "P*":"include", "RESTORED":"include"},
                "set":{"RESTORED":"configured", "PROJECT":"configured-project"}
            }}),
            &json!({"shell_environment_policy.filters.\"DROP?\"":"exclude"}),
        )
        .unwrap();
        let delta = BTreeMap::from([
            ("PROJECT".into(), Some("direnv-project".into())),
            ("PROJECT_SECRET".into(), Some("forbidden-secret".into())),
            ("Aws_Credential".into(), Some("forbidden-aws".into())),
            ("OUTSIDE".into(), Some("forbidden-outside".into())),
            ("DROP1".into(), Some("forbidden-drop".into())),
            ("PATH".into(), Some("project-path".into())),
            ("RESTORED".into(), None),
        ]);
        let filtered = policy.apply(&delta);
        assert_eq!(filtered["PROJECT"].as_deref(), Some("configured-project"));
        assert_eq!(filtered["RESTORED"].as_deref(), Some("configured"));
        for key in ["PROJECT_SECRET", "Aws_Credential", "OUTSIDE", "DROP1"] {
            assert_eq!(filtered[key], None);
        }
        assert_eq!(filtered["PATH"].as_deref(), Some("project-path"));
        let bootstrap = policy
            .bootstrap(
                effective,
                &json!({"set":{"BASH_ENV":"hook","ZDOTDIR":"dir","SHLVL":"1"}}),
            )
            .unwrap();
        assert_eq!(bootstrap["inherit"], "all");
        assert!(
            bootstrap["exclude"]
                .as_array()
                .unwrap()
                .contains(&json!("aws_*"))
        );
        assert!(
            bootstrap["include_only"]
                .as_array()
                .unwrap()
                .contains(&json!("BASH_ENV"))
        );
        assert!(!bootstrap.to_string().contains("direnv-project"));
    }

    #[test]
    fn restrictive_inheritance_and_legacy_filters_are_preserved() {
        let delta = BTreeMap::from([
            ("PATH".into(), Some("path".into())),
            ("PROJECT".into(), Some("project".into())),
        ]);
        for (inherit, keep_path) in [("none", false), ("core", true)] {
            let (policy, _) = Policy::resolve(
                &json!({"shell_environment_policy":{"inherit":inherit}}),
                &Value::Null,
            )
            .unwrap();
            let filtered = policy.apply(&delta);
            assert_eq!(filtered["PATH"].is_some(), keep_path);
            assert_eq!(filtered["PROJECT"], None);
        }
        let (policy, _) = Policy::resolve(&json!({"shell_environment_policy":{"exclude":["pr?ject"],"include_only":["P*"],"set":null,"filters":null}}), &Value::Null).unwrap();
        assert_eq!(policy.apply(&delta)["PROJECT"], None);
        assert!(
            Policy::resolve(
                &json!({"shell_environment_policy":{"futureRestriction":true}}),
                &Value::Null
            )
            .is_err()
        );
    }

    #[test]
    fn shell_scrubbing_matches_policy_and_preserves_runtime_proxy_values() {
        let (policy, _) = Policy::resolve(&json!({"shell_environment_policy": {
            "ignore_default_excludes":false,"exclude":["aws_*","DROP?"],
            "include_only":["PATH","PROJECT","RESTORED","HTTP_PROXY","*SECRET*","aws_*","DROP?"],
            "set":{"RESTORED":"intentional"}
        }}), &Value::Null).unwrap();
        let delta = BTreeMap::from([
            ("PROJECT".into(), Some("ok".into())),
            ("AWS_NEW".into(), Some("must-not-persist".into())),
        ]);
        let script =
            policy.scrub_script() + &super::super::environment::script(&policy.apply(&delta));
        assert!(!script.contains("must-not-persist"));
        let root = tempfile::tempdir().unwrap();
        let settings = super::super::environment::persist(root.path(), &script).unwrap();
        for shell in ["bash", "zsh"] {
            let Some(bin) = crate::agents::find_in_path(shell) else {
                continue;
            };
            for login in [false, true] {
                let output = std::process::Command::new(&bin)
                    .arg(if login { "-lc" } else { "-c" })
                    .arg("test \"$PROJECT\" = ok && test \"$RESTORED\" = intentional && test -z \"${AWS_NEW+x}${aws_old+x}${DROP1+x}${PROJECT_SECRET+x}${OUTSIDE+x}\" && test \"$HTTP_PROXY\" = managed-proxy")
                    .env_clear().env("PATH", "/bin:/usr/bin")
                    .env("BASH_ENV", settings["set"]["BASH_ENV"].as_str().unwrap())
                    .env("ZDOTDIR", settings["set"]["ZDOTDIR"].as_str().unwrap())
                    .env("SHLVL", "1").env("aws_old", "excluded").env("DROP1", "excluded")
                    .env("PROJECT_SECRET", "excluded").env("OUTSIDE", "excluded")
                    .env("HTTP_PROXY", "managed-proxy").env("CODEX_NETWORK_PROXY_ACTIVE", "1")
                    .output().unwrap();
                assert!(
                    output.status.success(),
                    "{shell} login={login}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}
