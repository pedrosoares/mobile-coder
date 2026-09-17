//! Name resolution inside the guest.
//!
//! A Linux userland resolves names through `/etc/resolv.conf`. Android has no such
//! file - it tracks DNS servers per network and exposes them to apps through
//! `ConnectivityManager` - and an Alpine minirootfs ships without one. So out of
//! the box the guest cannot resolve anything, and `apk update` fails with the
//! unhelpful *"temporary error (try again later)"*.
//!
//! The file is written from the phone's *actual* servers rather than a hardcoded
//! public resolver. Networks that filter or intercept DNS - captive portals,
//! corporate Wi-Fi, carriers - only work through their own resolvers, and quietly
//! routing a user's lookups to a third party is not a decision to make for them.
//! Public resolvers are the fallback only when Android reports none at all.
//!
//! Phones change networks constantly, so this is rewritten whenever the servers
//! change, not once at install.

use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
};

/// Used only when the platform reports no DNS servers at all.
pub const FALLBACK_SERVERS: [&str; 2] = ["1.1.1.1", "8.8.8.8"];

/// Parse server addresses as reported by Android, keeping only valid IPs.
///
/// Validation is the point: these strings end up in a file the guest resolver
/// parses, so anything that is not a plain IP address is dropped rather than
/// written. IPv6 link-local addresses carrying a zone (`fe80::1%wlan0`) are
/// dropped too - they are not usable from inside the guest.
pub fn parse_servers(raw: &str) -> Vec<IpAddr> {
    let mut servers: Vec<IpAddr> = raw
        .split([',', ' ', '\n'])
        .map(str::trim)
        .map(|s| s.trim_start_matches('/'))
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<IpAddr>().ok())
        .collect();
    servers.dedup();
    servers
}

/// Render `resolv.conf`. glibc and musl both read at most three `nameserver`
/// lines, so extras are not written.
pub fn render(servers: &[IpAddr]) -> String {
    let mut out = String::from("# Written by mobile-coder from the device's current network.\n");
    if servers.is_empty() {
        out.push_str("# The platform reported no DNS servers; using public fallbacks.\n");
        for fallback in FALLBACK_SERVERS {
            out.push_str(&format!("nameserver {fallback}\n"));
        }
    } else {
        for server in servers.iter().take(3) {
            out.push_str(&format!("nameserver {server}\n"));
        }
    }
    out
}

fn resolv_conf(rootfs: &Path) -> PathBuf {
    rootfs.join("etc/resolv.conf")
}

/// Write the guest's `resolv.conf`. Returns whether the file changed, so callers
/// can log only real updates.
pub fn write_resolv_conf(rootfs: &Path, servers: &[IpAddr]) -> std::io::Result<bool> {
    let path = resolv_conf(rootfs);
    let contents = render(servers);
    if fs::read_to_string(&path).ok().as_deref() == Some(contents.as_str()) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Write-then-rename, so a guest process never reads a half-written file.
    let staging = path.with_extension("tmp");
    fs::write(&staging, contents)?;
    fs::rename(&staging, &path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_android_actually_reports() {
        // Verbatim shape from `dumpsys connectivity` on a Galaxy Z Fold6.
        let raw = "/192.0.0.30,/2001:12e0:0:1025:a080::115,/2001:12e0:0:1025:a080::215";
        let servers = parse_servers(raw);
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].to_string(), "192.0.0.30");
        assert!(servers[1].is_ipv6());
    }

    #[test]
    fn anything_that_is_not_a_plain_ip_is_dropped() {
        let raw = "8.8.8.8, fe80::1%wlan0, not-an-ip, 1.1.1.1\noptions ndots:9, ";
        let servers = parse_servers(raw);
        let as_text: Vec<String> = servers.iter().map(ToString::to_string).collect();
        assert_eq!(as_text, ["8.8.8.8", "1.1.1.1"]);
    }

    #[test]
    fn a_crafted_value_cannot_inject_resolver_directives() {
        let rendered = render(&parse_servers("1.2.3.4\nsearch evil.example"));
        assert!(!rendered.contains("search"), "{rendered}");
        assert!(rendered.contains("nameserver 1.2.3.4"));
    }

    #[test]
    fn falls_back_to_public_resolvers_only_when_none_are_reported() {
        let rendered = render(&[]);
        assert!(rendered.contains("nameserver 1.1.1.1"));
        assert!(rendered.contains("nameserver 8.8.8.8"));
    }

    #[test]
    fn writes_at_most_three_nameservers() {
        let rendered = render(&parse_servers("1.1.1.1,2.2.2.2,3.3.3.3,4.4.4.4"));
        assert_eq!(rendered.matches("nameserver").count(), 3);
    }

    #[test]
    fn rewrites_only_when_the_servers_change() {
        let root = std::env::temp_dir().join("mc-dns-test");
        let _ = fs::remove_dir_all(&root);
        let a = parse_servers("1.1.1.1");
        let b = parse_servers("9.9.9.9");

        assert!(write_resolv_conf(&root, &a).unwrap(), "first write");
        assert!(!write_resolv_conf(&root, &a).unwrap(), "unchanged");
        assert!(write_resolv_conf(&root, &b).unwrap(), "network changed");
        assert!(fs::read_to_string(root.join("etc/resolv.conf")).unwrap().contains("9.9.9.9"));
        let _ = fs::remove_dir_all(&root);
    }
}
