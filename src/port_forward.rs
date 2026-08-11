//! Per-host ssh port forwards — the `Ports` field in the hosts panel (`Space h`),
//! parsed here and spliced onto the transport's forward child in
//! [`crate::backend`].
//!
//! A spec is *typed* before it ever reaches ssh, and that is the whole reason
//! this module exists rather than the field being passed through as text. The
//! forwards ride the same `ssh -N -L` child that carries the wire protocol, and
//! the two failure modes are not alike: a forward that fails to **bind** (port
//! taken, remote refuses) is a warning ssh prints and carries on from — we never
//! set `ExitOnForwardFailure`, precisely so a busy port can't take the dashboard
//! down with it — whereas a spec ssh can't **parse** is a usage error that exits
//! the child instantly. That child *is* the transport, so the connection task
//! would read it as a dead link and re-dial forever, once per backoff tick,
//! with nothing anywhere saying why. So only specs that parsed here are ever
//! passed on, and the panel draws the rest in red instead of dropping them
//! silently.
//!
//! The grammar is ssh's own, plus two shorthands, because the case a dashboard
//! actually has is "let me open the dev server on that box in my browser":
//! `3000` means `3000:localhost:3000`, and `8080:3000` means
//! `8080:localhost:3000`. Direction is an `L:`/`R:`/`D:` prefix — or a pasted
//! `-L`, since the text people have to hand is usually a fragment of an ssh
//! command line — and defaults to local, which is what is wanted nearly every
//! time.

use std::fmt;

/// Which way a forward runs: ssh's `-L` / `-R` / `-D`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// `-L`: listen here, connect out from the host.
    Local,
    /// `-R`: listen on the host, connect out from here.
    Remote,
    /// `-D`: a SOCKS proxy listening here, routed through the host.
    Dynamic,
}

impl Direction {
    /// The ssh flag this direction is requested with.
    fn flag(self) -> &'static str {
        match self {
            Direction::Local => "-L",
            Direction::Remote => "-R",
            Direction::Dynamic => "-D",
        }
    }

    /// The one letter the panel prefixes a forward with. Same letter as the
    /// flag, so what is displayed is readable back into what ssh was told.
    fn letter(self) -> char {
        match self {
            Direction::Local => 'L',
            Direction::Remote => 'R',
            Direction::Dynamic => 'D',
        }
    }
}

/// One parsed, well-formed forward.
///
/// Held decomposed rather than as the string it came from so that [`Self::spec`]
/// emits ssh's canonical four-field form no matter which shorthand was typed —
/// which in turn is what lets `-O cancel` name the *same* forward later (the
/// master matches on the spec it was given).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PortForward {
    dir: Direction,
    /// Address the listener binds to, when the user named one. `None` leaves it
    /// to ssh — loopback, unless `GatewayPorts` says otherwise. Deliberately not
    /// defaulted here: writing `localhost` in would override a host's own
    /// `GatewayPorts yes`, quietly breaking the one case that wants a wildcard.
    bind: Option<String>,
    /// Port the listener binds.
    port: u16,
    /// Where the other end connects to. `None` for [`Direction::Dynamic`], which
    /// learns its destination per-connection from the SOCKS handshake.
    dest: Option<(String, u16)>,
}

impl PortForward {
    /// The ssh flag and the argument that follows it — the pair spliced onto the
    /// forward child's argv, and onto `-O cancel`.
    pub(crate) fn flag(&self) -> &'static str {
        self.dir.flag()
    }

    /// The argument to [`Self::flag`], in ssh's canonical form.
    pub(crate) fn spec(&self) -> String {
        let bind = match &self.bind {
            Some(b) => format!("{b}:"),
            None => String::new(),
        };
        match &self.dest {
            Some((host, hostport)) => format!("{bind}{}:{host}:{hostport}", self.port),
            None => format!("{bind}{}", self.port),
        }
    }
}

impl fmt::Display for PortForward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.dir.letter(), self.spec())
    }
}

/// Parse one spec, or say why it isn't one.
///
/// The error is written to be read on a popup row, so it is a fragment naming
/// the problem rather than a sentence — it renders after a red `!` beside the
/// text the user typed, which is already the other half of the message.
pub(crate) fn parse(raw: &str) -> Result<PortForward, String> {
    let (dir, body) = split_direction(raw.trim());
    let body = body.trim();
    if body.is_empty() {
        return Err("no port".to_string());
    }
    let fields = split_fields(body).ok_or_else(|| "unbalanced []".to_string())?;
    if dir == Direction::Dynamic {
        let (bind, port) = match fields.as_slice() {
            [p] => (None, port_of(p)?),
            [b, p] => (Some(host_of(b)?), port_of(p)?),
            _ => return Err("expected [bind:]port".to_string()),
        };
        return Ok(PortForward {
            dir,
            bind,
            port,
            dest: None,
        });
    }
    // The two shorthands sit where ssh has no syntax at all (one and two
    // fields), so they extend the grammar rather than shadowing part of it: a
    // three- or four-field spec still means exactly what `man ssh` says.
    let (bind, port, dest) = match fields.as_slice() {
        [p] => (None, port_of(p)?, ("localhost".to_string(), port_of(p)?)),
        [a, b] => (None, port_of(a)?, ("localhost".to_string(), port_of(b)?)),
        [p, h, hp] => (None, port_of(p)?, (host_of(h)?, port_of(hp)?)),
        [b, p, h, hp] => (Some(host_of(b)?), port_of(p)?, (host_of(h)?, port_of(hp)?)),
        _ => return Err("expected [bind:]port:host:hostport".to_string()),
    };
    Ok(PortForward {
        dir,
        bind,
        port,
        dest: Some(dest),
    })
}

/// Split the panel's one-line field into individual specs.
///
/// Separated by commas *or* whitespace, since both read naturally in a single
/// text field and neither can occur inside a spec. A bare `-L`/`-R`/`-D` glues
/// onto the token after it: pasting `-L 8080:localhost:3000` out of a shell
/// history is the likeliest way this field ever gets filled, and splitting it
/// into two tokens would turn it into one unparseable spec plus one bare flag.
pub(crate) fn parse_list(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pending: Option<&str> = None;
    for tok in text
        .split([',', ' ', '\t', '\n', '\r'])
        .filter(|t| !t.is_empty())
    {
        if matches!(tok.to_ascii_lowercase().as_str(), "-l" | "-r" | "-d") {
            pending = Some(tok);
            continue;
        }
        out.push(match pending.take() {
            Some(flag) => format!("{flag}{tok}"),
            None => tok.to_string(),
        });
    }
    out
}

/// The specs that parsed, in order — what a backend is actually built with.
///
/// Malformed entries are dropped here rather than at the seam that spawns ssh
/// (see the module doc for why they must never reach it). They stay in
/// `hosts.json` untouched: the panel is where a typo is visible and fixable, and
/// rewriting the user's text out from under them on load would take that away.
pub(crate) fn valid(specs: &[String]) -> Vec<PortForward> {
    specs
        .iter()
        .filter_map(|s| match parse(s) {
            Ok(f) => Some(f),
            Err(why) => {
                tracing::warn!(
                    target: "captain_miao::ssh",
                    "ignoring malformed port forward `{s}`: {why}"
                );
                None
            }
        })
        .collect()
}

/// Peel an optional direction off the front, defaulting to local.
fn split_direction(s: &str) -> (Direction, &str) {
    // A pasted ssh flag, glued (`-L8080:…`) or joined by [`parse_list`].
    if let Some(head) = s.get(..2) {
        for (flag, dir) in [
            ("-l", Direction::Local),
            ("-r", Direction::Remote),
            ("-d", Direction::Dynamic),
        ] {
            if head.eq_ignore_ascii_case(flag) {
                return (dir, &s[2..]);
            }
        }
    }
    // A `L:` / `remote:` / `socks:` prefix. `localhost:8080:…` is safe from this
    // — the match is on the whole field, and `localhost` is not `local`.
    if let Some((head, rest)) = s.split_once(':') {
        let dir = match head.trim().to_ascii_lowercase().as_str() {
            "l" | "local" => Some(Direction::Local),
            "r" | "remote" | "reverse" => Some(Direction::Remote),
            "d" | "dynamic" | "socks" => Some(Direction::Dynamic),
            _ => None,
        };
        if let Some(dir) = dir {
            return (dir, rest);
        }
    }
    (Direction::Local, s)
}

/// Split on `:`, honouring the brackets an IPv6 literal is written in — ssh
/// spells those `[::1]:8080:…`, and a naive split turns one address into four
/// empty fields.
fn split_fields(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '[' => {
                depth += 1;
                cur.push(c);
            }
            ']' => {
                depth = depth.checked_sub(1)?;
                cur.push(c);
            }
            ':' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if depth != 0 {
        return None;
    }
    out.push(cur);
    Some(out)
}

fn port_of(s: &str) -> Result<u16, String> {
    s.trim()
        .parse::<u16>()
        .map_err(|_| format!("`{s}` is not a port"))
}

fn host_of(s: &str) -> Result<String, String> {
    let h = s.trim();
    if h.is_empty() {
        return Err("empty host".to_string());
    }
    Ok(h.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the field is for: the two shorthands, spelled out into the full form
    /// ssh is handed. `3000` and `8080:3000` are the whole reason a user doesn't
    /// have to know ssh's four-field syntax to reach a remote dev server.
    #[test]
    fn shorthands_expand_to_the_canonical_form() {
        assert_eq!(parse("3000").unwrap().spec(), "3000:localhost:3000");
        assert_eq!(parse("8080:3000").unwrap().spec(), "8080:localhost:3000");
        // Three and four fields keep ssh's own meaning, untouched.
        assert_eq!(parse("8080:db:5432").unwrap().spec(), "8080:db:5432");
        assert_eq!(
            parse("127.0.0.1:8080:db:5432").unwrap().spec(),
            "127.0.0.1:8080:db:5432"
        );
        // Every form is local unless it says otherwise.
        for s in ["3000", "8080:3000", "8080:db:5432", "0.0.0.0:8080:db:5432"] {
            assert_eq!(parse(s).unwrap().flag(), "-L", "{s}");
        }
    }

    #[test]
    fn a_direction_prefix_picks_the_flag() {
        assert_eq!(parse("R:9000").unwrap().flag(), "-R");
        assert_eq!(parse("remote:9000:localhost:22").unwrap().flag(), "-R");
        assert_eq!(parse("d:1080").unwrap().flag(), "-D");
        assert_eq!(parse("socks:1080").unwrap().flag(), "-D");
        // A pasted flag, glued or split — `parse_list` produces the glued form.
        assert_eq!(parse("-R9000").unwrap().flag(), "-R");
        assert_eq!(parse("-l8080:3000").unwrap().spec(), "8080:localhost:3000");
        // Dynamic takes no destination, and one optional bind address.
        assert_eq!(parse("d:1080").unwrap().spec(), "1080");
        assert_eq!(parse("d:127.0.0.1:1080").unwrap().spec(), "127.0.0.1:1080");
    }

    /// `localhost` starts with `local` — the prefix match is on the whole field
    /// before the first colon, so an ordinary bind address can't be eaten as a
    /// direction.
    #[test]
    fn a_bind_address_is_not_mistaken_for_a_direction() {
        let f = parse("localhost:8080:db:5432").unwrap();
        assert_eq!(f.flag(), "-L");
        assert_eq!(f.spec(), "localhost:8080:db:5432");
    }

    /// ssh writes IPv6 literals bracketed; splitting on a bare `:` would shred
    /// one address into four empty fields and reject a legal spec.
    #[test]
    fn ipv6_literals_survive_the_split() {
        assert_eq!(
            parse("[::1]:8080:[fe80::1]:5432").unwrap().spec(),
            "[::1]:8080:[fe80::1]:5432"
        );
        assert!(parse("[::1:8080").is_err());
    }

    /// The specs that must never reach ssh: each of these is a *usage* error
    /// there, which exits the forward child and takes the transport with it.
    #[test]
    fn malformed_specs_are_rejected_rather_than_passed_on() {
        for bad in [
            "",
            "  ",
            "http",
            "8080:",
            ":3000",
            "80x0",
            "99999",
            "a:b:c:d:e",
            "8080:db:http",
            "d:1:2:3",
        ] {
            assert!(parse(bad).is_err(), "expected `{bad}` to be rejected");
        }
        // …and `valid` is the filter that enforces it, keeping the good ones.
        let specs = vec![
            "3000".to_string(),
            "nonsense".to_string(),
            "R:22".to_string(),
        ];
        let kept = valid(&specs);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].spec(), "3000:localhost:3000");
        assert_eq!(kept[1].flag(), "-R");
    }

    #[test]
    fn the_field_splits_on_commas_and_whitespace() {
        assert_eq!(
            parse_list("3000, 8080:3000  R:9000"),
            vec!["3000", "8080:3000", "R:9000"]
        );
        assert_eq!(parse_list("   "), Vec::<String>::new());
        // A pasted ssh fragment: the flag glues onto the spec that follows it,
        // instead of becoming a token of its own that parses as nothing.
        assert_eq!(
            parse_list("-L 8080:localhost:3000 -R 9000"),
            vec!["-L8080:localhost:3000", "-R9000"]
        );
        for s in parse_list("-L 8080:localhost:3000 -R 9000") {
            assert!(parse(&s).is_ok(), "{s}");
        }
    }

    /// The panel line and the argv have to agree — `Display` is the readable
    /// form of exactly the pair ssh is handed.
    #[test]
    fn display_reads_back_as_what_ssh_was_told() {
        let f = parse("R:9000:localhost:22").unwrap();
        assert_eq!(f.to_string(), "R 9000:localhost:22");
        assert_eq!((f.flag(), f.spec().as_str()), ("-R", "9000:localhost:22"));
        assert_eq!(parse("1080").unwrap().to_string(), "L 1080:localhost:1080");
        assert_eq!(parse("d:1080").unwrap().to_string(), "D 1080");
    }
}
