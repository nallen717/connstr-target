// ctarget: given a connection string, print what it actually resolves to.
//
// Connection strings hide their real behavior in ways that bite people:
// a missing port silently falls back to a scheme default, a comma-separated
// host list is tried in order rather than load-balanced, and a query
// parameter can quietly turn TLS on or off. This tool makes that explicit
// instead of making you read driver source to find out.

use std::env;
use std::io::{self, Read};
use std::process;

struct Parsed {
    scheme: String,
    username: Option<String>,
    password_present: bool,
    hosts: Vec<(String, Option<u16>, bool)>, // (host, resolved port, was explicit)
    database: Option<String>,
    params: Vec<(String, String)>,
    tls_note: Option<String>,
}

fn main() {
    let arg = env::args().nth(1);
    if let Some(a) = &arg {
        if a == "--help" || a == "-h" {
            print_usage();
            return;
        }
    }

    let input = match arg {
        Some(s) => s,
        None => {
            let mut buf = String::new();
            if io::stdin().read_to_string(&mut buf).is_err() || buf.trim().is_empty() {
                print_usage();
                process::exit(2);
            }
            buf
        }
    };

    match parse(input.trim()) {
        Ok(p) => print_report(&p),
        Err(e) => {
            eprintln!("error: {}", e);
            process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!("usage: ctarget <connection-string>");
    eprintln!("       echo <connection-string> | ctarget");
    eprintln!();
    eprintln!("accepts either a URI-style string or a libpq key=value string:");
    eprintln!("  ctarget 'postgres://app:secret@db1:5432,db2:5432/orders?sslmode=require'");
    eprintln!("  ctarget \"host=db1,db2 port=5432 dbname=orders user=app sslmode=require\"");
}

// (default_port, tls_forced)
fn scheme_info(scheme: &str) -> Option<(u16, bool)> {
    Some(match scheme {
        "postgres" | "postgresql" => (5432, false),
        "mysql" => (3306, false),
        "mongodb" => (27017, false),
        "mongodb+srv" => (27017, true),
        "redis" => (6379, false),
        "rediss" => (6379, true),
        "amqp" => (5672, false),
        "amqps" => (5671, true),
        "http" => (80, false),
        "https" => (443, true),
        _ => return None,
    })
}

fn parse(input: &str) -> Result<Parsed, String> {
    if input.contains("://") {
        parse_uri(input)
    } else {
        parse_libpq(input)
    }
}

fn parse_uri(input: &str) -> Result<Parsed, String> {
    let (scheme, rest) = input
        .split_once("://")
        .ok_or_else(|| "missing scheme (expected something like scheme://...)".to_string())?;
    let scheme = scheme.to_lowercase();
    if scheme.is_empty() {
        return Err("empty scheme before '://'".to_string());
    }

    let info = scheme_info(&scheme);
    let default_port = info.map(|(p, _)| p);
    let forced_tls = info.map(|(_, t)| t).unwrap_or(false);

    // authority runs up to the first '/' or '?'
    let authority_end = rest
        .char_indices()
        .find(|&(_, c)| c == '/' || c == '?')
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];

    let (userinfo, hostlist) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };

    let (username, password_present) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((user, _pass)) => (Some(url_decode(user)), true),
            None => (Some(url_decode(u)), false),
        },
        None => (None, false),
    };

    if hostlist.trim().is_empty() {
        return Err("no host found after scheme".to_string());
    }

    let mut hosts = Vec::new();
    for hp in hostlist.split(',').filter(|s| !s.is_empty()) {
        let (host, port) = split_host_port(hp)?;
        let explicit = port.is_some();
        let resolved = port.or(default_port);
        hosts.push((host, resolved, explicit));
    }

    let (path_part, query_part) = match tail.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (tail.as_ref(), None),
    };
    let database = {
        let d = path_part.trim_start_matches('/');
        if d.is_empty() {
            None
        } else {
            Some(url_decode(d))
        }
    };

    let params = query_part.map(parse_query).unwrap_or_default();
    let tls_note = tls_note(&scheme, forced_tls, &params);

    Ok(Parsed {
        scheme,
        username,
        password_present,
        hosts,
        database,
        params,
        tls_note,
    })
}

// libpq key=value strings have no scheme; postgres is the only driver family
// that uses this format, so the defaults below match postgres's own.
fn parse_libpq(input: &str) -> Result<Parsed, String> {
    let pairs = tokenize_libpq(input)?;
    if pairs.is_empty() {
        return Err("empty connection string".to_string());
    }

    let mut host_str: Option<String> = None;
    let mut port_str: Option<String> = None;
    let mut dbname: Option<String> = None;
    let mut user: Option<String> = None;
    let mut password_present = false;
    let mut params = Vec::new();

    for (k, v) in pairs {
        match k.as_str() {
            "host" | "hostaddr" => host_str = Some(v),
            "port" => port_str = Some(v),
            "dbname" => dbname = Some(v),
            "user" => user = Some(v),
            "password" => password_present = true,
            _ => params.push((k, v)),
        }
    }

    let host_list: Vec<&str> = match &host_str {
        Some(h) if !h.is_empty() => h.split(',').collect(),
        _ => Vec::new(),
    };
    let port_list: Vec<&str> = match &port_str {
        Some(p) if !p.is_empty() => p.split(',').collect(),
        _ => Vec::new(),
    };

    if port_list.len() > 1 && host_list.len() > 1 && port_list.len() != host_list.len() {
        return Err(format!(
            "{} host(s) but {} port(s); libpq needs one port for all hosts or one per host",
            host_list.len(),
            port_list.len()
        ));
    }

    let mut hosts = Vec::new();
    if host_list.is_empty() {
        // libpq with no host= falls back to a local unix-domain socket,
        // not to "localhost" - see the unix socket item on the roadmap.
        let port = port_list.first().and_then(|p| p.parse::<u16>().ok()).or(Some(5432));
        hosts.push(("(unix socket, default path not resolved)".to_string(), port, false));
    } else {
        for (i, h) in host_list.iter().enumerate() {
            let port_text = if port_list.len() == 1 {
                Some(port_list[0])
            } else {
                port_list.get(i).copied()
            };
            let port = match port_text {
                Some(p) if !p.is_empty() => Some(
                    p.parse::<u16>()
                        .map_err(|_| format!("bad port '{}'", p))?,
                ),
                _ => None,
            };
            let explicit = port.is_some();
            let resolved = port.or(Some(5432));
            hosts.push((h.to_string(), resolved, explicit));
        }
    }

    let tls_note = tls_note("postgres", false, &params);

    Ok(Parsed {
        scheme: "postgres (libpq key=value)".to_string(),
        username: user,
        password_present,
        hosts,
        database: dbname,
        params,
        tls_note,
    })
}

fn tokenize_libpq(input: &str) -> Result<Vec<(String, String)>, String> {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;

    while i < n {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }

        let key_start = i;
        while i < n && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n || chars[i] != '=' {
            let bad: String = chars[key_start..i].iter().collect();
            return Err(format!("expected 'key=value', found '{}'", bad));
        }
        let key: String = chars[key_start..i].iter().collect();
        if key.is_empty() {
            return Err("empty key in connection string".to_string());
        }
        i += 1; // skip '='

        let mut value = String::new();
        if i < n && chars[i] == '\'' {
            i += 1;
            let mut closed = false;
            while i < n {
                match chars[i] {
                    '\\' if i + 1 < n => {
                        value.push(chars[i + 1]);
                        i += 2;
                    }
                    '\'' => {
                        closed = true;
                        i += 1;
                        break;
                    }
                    c => {
                        value.push(c);
                        i += 1;
                    }
                }
            }
            if !closed {
                return Err(format!("unterminated quoted value for key '{}'", key));
            }
        } else {
            while i < n && !chars[i].is_whitespace() {
                if chars[i] == '\\' && i + 1 < n {
                    value.push(chars[i + 1]);
                    i += 2;
                } else {
                    value.push(chars[i]);
                    i += 1;
                }
            }
        }

        out.push((key.to_lowercase(), value));
    }

    Ok(out)
}

fn split_host_port(hp: &str) -> Result<(String, Option<u16>), String> {
    let hp = hp.trim();
    if let Some(rest) = hp.strip_prefix('[') {
        // IPv6 literal: [::1]:5432
        let end = rest
            .find(']')
            .ok_or_else(|| format!("unterminated '[' in host '{}'", hp))?;
        let host = rest[..end].to_string();
        let after = &rest[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(
                p.parse::<u16>()
                    .map_err(|_| format!("bad port '{}' in '{}'", p, hp))?,
            ),
            None => None,
        };
        return Ok((host, port));
    }

    match hp.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            let port = p
                .parse::<u16>()
                .map_err(|_| format!("bad port '{}' in '{}'", p, hp))?;
            Ok((h.to_string(), Some(port)))
        }
        _ => Ok((hp.to_string(), None)),
    }
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (url_decode(k), url_decode(v)),
            None => (url_decode(kv), String::new()),
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn tls_note(scheme: &str, forced_tls: bool, params: &[(String, String)]) -> Option<String> {
    if forced_tls {
        return Some(format!("on, implied by scheme '{}'", scheme));
    }
    for (k, v) in params {
        let kl = k.to_lowercase();
        let vl = v.to_lowercase();
        if kl == "sslmode" && vl != "disable" {
            return Some(format!("likely on, sslmode={}", v));
        }
        if (kl == "ssl" || kl == "tls")
            && matches!(vl.as_str(), "true" | "1" | "require" | "required")
        {
            return Some(format!("likely on, {}={}", k, v));
        }
    }
    None
}

fn print_report(p: &Parsed) {
    println!("scheme:   {}", p.scheme);
    match &p.username {
        Some(u) if p.password_present => println!("auth:     {}:*** (password present in string)", u),
        Some(u) => println!("auth:     {} (no password)", u),
        None => println!("auth:     none"),
    }

    println!("hosts:    {} target(s), tried in order", p.hosts.len());
    for (i, (host, port, explicit)) in p.hosts.iter().enumerate() {
        match port {
            Some(port) if *explicit => println!("  {}. {}:{}", i + 1, host, port),
            Some(port) => println!("  {}. {}:{} (default for scheme)", i + 1, host, port),
            None => println!("  {}. {} (no port given, no known default for this scheme)", i + 1, host),
        }
    }

    match &p.database {
        Some(d) => println!("database: {}", d),
        None => println!("database: (none specified)"),
    }

    match &p.tls_note {
        Some(t) => println!("tls:      {}", t),
        None => println!("tls:      no indication in string"),
    }

    if !p.params.is_empty() {
        println!("params:");
        for (k, v) in &p.params {
            println!("  {} = {}", k, v);
        }
    }
}
