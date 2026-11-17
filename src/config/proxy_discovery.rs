//! Bounded, protocol-aware discovery of a local proxy.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_millis(450);

/// Discover a usable local proxy without reading subscriptions, credentials,
/// command lines, or changing user configuration.
pub fn discover_local_proxy() -> Option<String> {
    #[cfg(windows)]
    {
        // A running Clash Verge core is the strongest local signal. Prefer
        // its process-owned listeners over inherited environment variables.
        for port in windows_clash_verge_listener_ports() {
            if let Some(proxy) = verify_loopback_port(port) {
                return Some(proxy);
            }
        }
    }

    for key in ["GPTGUARD_PROXY", "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
        let Some(value) = std::env::var(key).ok().and_then(normalize_env_proxy) else {
            continue;
        };
        if verify_proxy_url(&value) {
            return Some(value);
        }
    }

    #[cfg(windows)]
    {
        if let Some(proxy) = windows_system_proxy() {
            if verify_proxy_url(&proxy) {
                return Some(proxy);
            }
        }
    }

    for port in [7890u16, 7891, 1080, 10808, 20170] {
        if let Some(proxy) = verify_loopback_port(port) {
            return Some(proxy);
        }
    }
    None
}

fn normalize_env_proxy(value: String) -> Option<String> {
    let value = value.trim();
    let parsed = url::Url::parse(value).ok()?;
    if parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.host_str().is_some()
        && parsed.port().is_some()
        && matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h")
    {
        Some(value.to_string())
    } else {
        None
    }
}

fn verify_proxy_url(value: &str) -> bool {
    let Ok(parsed) = url::Url::parse(value) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let Some(port) = parsed.port() else {
        return false;
    };
    if !is_loopback_host(host) || !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    match parsed.scheme() {
        "http" | "https" => probe_http_connect(host, port),
        "socks5" | "socks5h" => probe_socks5(host, port),
        _ => false,
    }
}

fn verify_loopback_port(port: u16) -> Option<String> {
    if probe_http_connect("127.0.0.1", port) {
        return Some(format!("http://127.0.0.1:{port}"));
    }
    if probe_socks5("127.0.0.1", port) {
        return Some(format!("socks5://127.0.0.1:{port}"));
    }
    None
}

fn connect(host: &str, port: u16) -> Option<TcpStream> {
    let addr = format!("{host}:{port}").parse().ok()?;
    let stream = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).ok()?;
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    Some(stream)
}

fn probe_http_connect(host: &str, port: u16) -> bool {
    let Some(mut stream) = connect(host, port) else {
        return false;
    };
    let request = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nProxy-Connection: close\r\n\r\n";
    if stream.write_all(request).is_err() {
        return false;
    }
    let mut response = Vec::with_capacity(256);
    let mut buf = [0u8; 512];
    while response.len() < 8192 {
        let read = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return false,
        };
        response.extend_from_slice(&buf[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let first_line = response
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let first_line = String::from_utf8_lossy(first_line).into_owned();
    let fields = first_line.split_whitespace().collect::<Vec<_>>();
    fields
        .get(1)
        .and_then(|status| status.parse::<u16>().ok())
        .is_some_and(|status| status / 100 == 2)
}

fn probe_socks5(host: &str, port: u16) -> bool {
    let Some(mut stream) = connect(host, port) else {
        return false;
    };
    if stream.write_all(&[0x05, 0x01, 0x00]).is_err() {
        return false;
    }
    let mut method = [0u8; 2];
    stream.read_exact(&mut method).is_ok() && method[0] == 0x05 && method[1] != 0xff
}

fn is_loopback_host(host: &str) -> bool {
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or_else(|_| host.eq_ignore_ascii_case("localhost"))
}

#[cfg(windows)]
fn windows_clash_verge_listener_ports() -> Vec<u16> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let Ok(mut system) = System::new_all() else {
        return Vec::new();
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let pids = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let name = process.name().to_string_lossy();
            let normalized = name.strip_suffix(".exe").unwrap_or(&name);
            matches!(
                normalized.to_ascii_lowercase().as_str(),
                "verge-mihomo" | "verge-mihomo-alpha"
            )
            .then_some(pid.as_u32())
        })
        .collect::<Vec<_>>();
    listener_ports_for_pids(&pids)
}

#[cfg(windows)]
fn windows_system_proxy() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
        .ok()?;
    if key.get_value::<u32, _>("ProxyEnable").ok()? == 0 {
        return None;
    }
    let raw: String = key.get_value("ProxyServer").ok()?;
    raw.split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && !entry.eq_ignore_ascii_case("<local>"))
        .filter_map(|entry| {
            let value = entry
                .split_once('=')
                .map(|(_, value)| value)
                .unwrap_or(entry)
                .trim();
            let candidate = if value.contains("://") {
                value.to_string()
            } else {
                format!("http://{value}")
            };
            let parsed = url::Url::parse(&candidate).ok()?;
            let host = parsed.host_str()?;
            (is_loopback_host(host)
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.port().is_some())
            .then_some(candidate)
        })
        .next()
}

#[cfg(windows)]
fn listener_ports_for_pids(pids: &[u32]) -> Vec<u16> {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Row {
        state: u32,
        local_addr: u32,
        local_port: u32,
        remote_addr: u32,
        remote_port: u32,
        pid: u32,
    }
    #[repr(C)]
    struct Table {
        count: u32,
        rows: [Row; 1],
    }
    #[link(name = "iphlpapi")]
    unsafe extern "system" {
        fn GetExtendedTcpTable(
            table: *mut std::ffi::c_void,
            size: *mut u32,
            order: i32,
            family: u32,
            class: u32,
            reserved: u32,
        ) -> u32;
    }
    const LISTENER: u32 = 3;
    const AF_INET: u32 = 2;
    const LISTEN: u32 = 2;
    const INSUFFICIENT_BUFFER: u32 = 122;

    let mut size = 0u32;
    let result =
        unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, AF_INET, LISTENER, 0) };
    if result != INSUFFICIENT_BUFFER || size < 4 {
        return Vec::new();
    }
    let mut buffer = vec![0u8; size as usize];
    let result = unsafe {
        GetExtendedTcpTable(
            buffer.as_mut_ptr().cast(),
            &mut size,
            0,
            AF_INET,
            LISTENER,
            0,
        )
    };
    if result != 0 {
        return Vec::new();
    }
    let table = unsafe { &*(buffer.as_ptr().cast::<Table>()) };
    let row_size = std::mem::size_of::<Row>();
    let max_rows = buffer.len().saturating_sub(4) / row_size;
    let row_count = (table.count as usize).min(max_rows);
    let rows = unsafe { std::slice::from_raw_parts(table.rows.as_ptr(), row_count) };
    rows.iter()
        .filter(|row| {
            row.state == LISTEN && u32::from_be(row.local_addr) == u32::from(Ipv4Addr::LOCALHOST)
        })
        .filter(|row| pids.contains(&row.pid))
        .map(|row| u16::from_be(row.local_port as u16))
        .filter(|port| *port != 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_credentials_and_non_proxy_schemes() {
        assert!(normalize_env_proxy("http://user:pass@127.0.0.1:7890".into()).is_none());
        assert!(normalize_env_proxy("ftp://127.0.0.1:7890".into()).is_none());
    }

    #[test]
    fn recognizes_loopback_only() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("example.com"));
    }
}
