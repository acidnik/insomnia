use std::time::Duration;

use anyhow::{anyhow, Result};
use regex::Regex;

/// Metadata parsed from `# key: value` comment lines in a check script.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub period: Option<String>,
    pub timeout: Option<String>,
    pub message: Option<String>,
    /// escalation schedule for repeated alerts
    pub repeat: Vec<String>,
    /// send a notification when the check recovers (default true)
    pub report_restored: bool,
    /// wait this long and retry once before declaring failure
    pub flake: Option<String>,
    /// re-check interval while the alert is active (overrides [defaults].recheck)
    pub recheck: Option<String>,
    /// human-readable display name used in alerts instead of the file id
    pub name: Option<String>,
    pub tags: Vec<String>,
    /// `var: name=value` pairs, passed to the check as environment variables
    pub vars: Vec<(String, String)>,
}

impl Meta {
    /// scan for lines like `#\s+(\w+): (.*)`, keep only known keys
    pub fn parse(text: &str) -> Meta {
        let re = Regex::new(r"(?m)^\s*#\s+(\w+):\s*(.*)$").expect("static regex");
        let mut meta = Meta {
            report_restored: true,
            ..Default::default()
        };
        for cap in re.captures_iter(text) {
            let key = &cap[1];
            let value = cap[2].trim();
            match key {
                "period" => meta.period = Some(value.to_string()),
                "timeout" => meta.timeout = Some(value.to_string()),
                "message" => meta.message = Some(value.to_string()),
                "repeat" => {
                    meta.repeat = value
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                "report_restored" => {
                    meta.report_restored =
                        !matches!(value.to_lowercase().as_str(), "false" | "no" | "0" | "off");
                }
                "flake" => meta.flake = Some(value.to_string()),
                "recheck" => meta.recheck = Some(value.to_string()),
                "name" => meta.name = Some(value.to_string()),
                "tags" => {
                    meta.tags = value
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                "var" => {
                    if let Some((k, v)) = value.split_once('=') {
                        meta.vars.push((k.trim().to_string(), v.trim().to_string()));
                    }
                }
                _ => {} // unknown keys are ignored
            }
        }
        meta
    }
}

/// Parse a duration like `30`, `30s`, `5m`, `1h`, `1h30m`, `2d`.
/// Bare numbers mean seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration"));
    }
    let mut secs: u64 = 0;
    let mut num = String::new();
    let mut matched = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else {
            let n: u64 = num.parse().map_err(|_| anyhow!("bad duration '{s}'"))?;
            num.clear();
            let mult = match ch {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86400,
                _ => return Err(anyhow!("bad duration unit '{ch}' in '{s}'")),
            };
            secs += n.saturating_mul(mult);
            matched = true;
        }
    }
    if !num.is_empty() {
        // trailing bare number = seconds
        let n: u64 = num.parse().map_err(|_| anyhow!("bad duration '{s}'"))?;
        secs += n;
        matched = true;
    }
    if !matched {
        return Err(anyhow!("bad duration '{s}'"));
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_all_known_keys() {
        let meta = Meta::parse(
            "#!/usr/bin/env bash\n\
             # tags: ssh, server, disk\n\
             # period: 1h\n\
             # timeout: 30s\n\
             # repeat: 30m, 1h, 6h\n\
             # report_restored: false\n\
             # flake: 1m\n\
             # var: dev=/dev/nvme*,/dev/sd*\n\
             # var: free_percent=5\n\
             # var: free_gb=10\n\
             # unknown: ignored\n\
             ssh site 'df -h' | parse_df\n",
        );
        assert_eq!(meta.period.as_deref(), Some("1h"));
        assert_eq!(meta.timeout.as_deref(), Some("30s"));
        assert_eq!(meta.repeat, vec!["30m", "1h", "6h"]);
        assert!(!meta.report_restored);
        assert_eq!(meta.flake.as_deref(), Some("1m"));
        assert_eq!(meta.tags, vec!["ssh", "server", "disk"]);
        assert_eq!(
            meta.vars,
            vec![
                ("dev".to_string(), "/dev/nvme*,/dev/sd*".to_string()),
                ("free_percent".to_string(), "5".to_string()),
                ("free_gb".to_string(), "10".to_string()),
            ]
        );
        assert_eq!(meta.message, None);
    }

    #[test]
    fn defaults_report_restored_true() {
        let meta = Meta::parse("# period: 5m\ncurl https://x\n");
        assert!(meta.report_restored);
    }

    #[test]
    fn parses_message_with_dollar_placeholders() {
        let meta = Meta::parse("# message: disk $name broken: $stderr\n");
        assert_eq!(meta.message.as_deref(), Some("disk $name broken: $stderr"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172800));
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("1x").is_err());
    }
}
