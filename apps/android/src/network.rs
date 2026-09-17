//! The device's current DNS servers, pushed from Kotlin.

use std::{net::IpAddr, path::Path, sync::RwLock};

static DNS_SERVERS: RwLock<Vec<IpAddr>> = RwLock::new(Vec::new());

/// Replace the known servers. Called on startup and on every network change.
pub fn set_dns_servers(raw: &str) {
    let servers = mc_sandbox::dns::parse_servers(raw);
    log::info!("dns servers: {servers:?}");
    if let Ok(mut slot) = DNS_SERVERS.write() {
        *slot = servers;
    }
    // Apply immediately if the guest already exists: a network switch mid-session
    // should not leave the guest pointing at the old network's resolvers.
    if let Some((_, rootfs)) = crate::agent_task::sandbox_paths() {
        apply(rootfs);
    }
}

/// Write the current servers into the guest's resolv.conf.
pub fn apply(rootfs: &Path) {
    let servers = DNS_SERVERS.read().map(|s| s.clone()).unwrap_or_default();
    match mc_sandbox::dns::write_resolv_conf(rootfs, &servers) {
        Ok(true) => log::info!("guest resolv.conf updated"),
        Ok(false) => {}
        Err(e) => log::error!("could not write guest resolv.conf: {e}"),
    }
}

#[cfg(target_os = "android")]
mod jni_bridge {
    use jni::{
        EnvUnowned,
        objects::{JClass, JString},
    };

    /// `MainActivity.nativeSetDnsServers`.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_net_pedrosoares_mobilecoder_MainActivity_nativeSetDnsServers<'caller>(
        mut unowned_env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        servers: JString<'caller>,
    ) {
        let outcome = unowned_env.with_env(|env| -> Result<(), jni::errors::Error> {
            super::set_dns_servers(&servers.try_to_string(env)?);
            Ok(())
        });
        outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
    }
}
