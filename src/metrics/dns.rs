//! DNS servers of the active interface: read without elevation, changed through one elevated
//! PowerShell command (one UAC prompt per change; WinStats itself stays unelevated).
//!
//! Reading: `GetAdaptersAddresses` lists the servers in use; whether they are set by hand or come
//! from DHCP is the interface's `NameServer` value in the Tcpip / Tcpip6 registry keys (set by
//! hand when not empty).
//!
//! Writing: `Set-DnsClientServerAddress` (IPv4 and IPv6 in one call) or `-ResetServerAddresses`
//! for automatic, then `Clear-DnsClientCache`, run with the "runas" verb.

use std::net::IpAddr;

/// A DNS choice offered in the network popup.
pub struct Preset {
    pub name: &'static str,
    pub servers: &'static [&'static str],
}

/// The list in the popup (Automatic and Custom come around it). IPv4 first, then IPv6.
pub const PRESETS: &[Preset] = &[
    Preset { name: "Cloudflare", servers: &["1.1.1.1", "1.0.0.1", "2606:4700:4700::1111", "2606:4700:4700::1001"] },
    Preset { name: "Google", servers: &["8.8.8.8", "8.8.4.4", "2001:4860:4860::8888", "2001:4860:4860::8844"] },
    Preset { name: "Quad9", servers: &["9.9.9.9", "149.112.112.112", "2620:fe::fe", "2620:fe::9"] },
    Preset { name: "AdGuard", servers: &["94.140.14.14", "94.140.15.15", "2a10:50c0::ad1:ff", "2a10:50c0::ad2:ff"] },
    Preset { name: "OpenDNS", servers: &["208.67.222.222", "208.67.220.220", "2620:119:35::35", "2620:119:53::53"] },
];

/// What is set on the interface now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DnsState {
    /// interface alias ("Wi-Fi")
    pub alias: String,
    /// servers in use, as Windows lists them
    pub servers: Vec<IpAddr>,
    /// from DHCP / router advertisements (nothing set by hand)
    pub automatic: bool,
}

/// What the popup should mark as chosen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Choice {
    Automatic,
    Preset(usize),
    Custom,
}

impl DnsState {
    /// Automatic, the preset whose IPv4 servers are the ones in use, or Custom.
    pub fn choice(&self) -> Choice {
        if self.automatic {
            return Choice::Automatic;
        }
        let mut v4: Vec<IpAddr> = self.servers.iter().filter(|s| s.is_ipv4()).cloned().collect();
        v4.sort();
        for (i, p) in PRESETS.iter().enumerate() {
            let mut pv4: Vec<IpAddr> = p.servers.iter().filter_map(|s| s.parse().ok()).filter(|s: &IpAddr| s.is_ipv4()).collect();
            pv4.sort();
            if !v4.is_empty() && v4 == pv4 {
                return Choice::Preset(i);
            }
        }
        Choice::Custom
    }
}

/// Addresses typed by the user: separated by commas, semicolons or spaces; 1 to 6, each a valid
/// IPv4 or IPv6 address. None when anything does not parse.
pub fn parse_servers(text: &str) -> Option<Vec<IpAddr>> {
    let list: Vec<&str> = text.split(|c: char| c == ',' || c == ';' || c.is_whitespace()).filter(|s| !s.is_empty()).collect();
    if list.is_empty() || list.len() > 6 {
        return None;
    }
    list.iter().map(|s| s.parse::<IpAddr>().ok()).collect()
}

/// The PowerShell that sets `servers` (None = automatic) on the interface `alias`. None when the
/// alias cannot be quoted safely.
pub fn command(alias: &str, servers: Option<&[IpAddr]>) -> Option<String> {
    if alias.is_empty() || alias.chars().any(|c| matches!(c, '"' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') || c.is_control()) {
        return None;
    }
    // single-quoted PowerShell string: a quote inside is doubled
    let a = alias.replace('\'', "''");
    let set = match servers {
        None => format!("Set-DnsClientServerAddress -InterfaceAlias '{a}' -ResetServerAddresses"),
        Some(list) if !list.is_empty() => {
            let l: Vec<String> = list.iter().map(|s| format!("'{s}'")).collect();
            format!("Set-DnsClientServerAddress -InterfaceAlias '{a}' -ServerAddresses @({})", l.join(","))
        }
        Some(_) => return None,
    };
    Some(format!("$ErrorActionPreference='Stop'; {set}; Clear-DnsClientCache"))
}

/// Run `cmd` elevated (one UAC prompt) without waiting. The process handle (as an integer, for
/// a waiting thread) on success; None when the user declined or it could not start.
pub fn run_elevated(cmd: &str) -> Option<isize> {
    use crate::util::{pcwstr, wide};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let verb = wide("runas");
    let file = wide("powershell.exe");
    let params = wide(&format!("-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -Command \"{cmd}\""));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: pcwstr(&verb),
        lpFile: pcwstr(&file),
        lpParameters: pcwstr(&params),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SHELLEXECUTEINFOW is packed on 32-bit Windows: the handle is copied out before use
    let process = unsafe {
        if ShellExecuteExW(&mut info).is_err() {
            crate::log!("dns: elevated command not started (declined?)");
            return None;
        }
        info.hProcess
    };
    if process.is_invalid() {
        crate::log!("dns: elevated command not started (declined?)");
        return None;
    }
    Some(process.0 as isize)
}

/// Wait for the elevated process; its exit code (0 = done).
pub fn wait_exit(process: isize) -> u32 {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    let h = HANDLE(process as *mut _);
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(h, INFINITE);
        let _ = GetExitCodeProcess(h, &mut code);
        let _ = CloseHandle(h);
    }
    code
}

/// A standard query for `name` (type A, class IN, recursion desired) with transaction `id`.
pub fn query_packet(id: u16, name: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(32);
    p.extend_from_slice(&id.to_be_bytes());
    // flags: recursion desired; one question, no other records
    p.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.') {
        p.push(label.len() as u8);
        p.extend_from_slice(label.as_bytes());
    }
    p.extend_from_slice(&[0, 0, 1, 0, 1]);
    p
}

/// One query to `server` (UDP 53): the round trip in ms, None without an answer within 1 s.
fn probe_once(server: IpAddr, id: u16) -> Option<f32> {
    use std::net::{SocketAddr, UdpSocket};
    use std::time::{Duration, Instant};
    let bind: SocketAddr = if server.is_ipv4() { ([0u8, 0, 0, 0], 0).into() } else { (std::net::Ipv6Addr::UNSPECIFIED, 0).into() };
    let sock = UdpSocket::bind(bind).ok()?;
    sock.connect(SocketAddr::new(server, 53)).ok()?;
    let packet = query_packet(id, "example.com");
    let t0 = Instant::now();
    sock.send(&packet).ok()?;
    let mut buf = [0u8; 512];
    loop {
        let left = Duration::from_millis(1000).checked_sub(t0.elapsed()).filter(|d| !d.is_zero())?;
        sock.set_read_timeout(Some(left)).ok()?;
        let n = sock.recv(&mut buf).ok()?;
        // the answer to this query: same id, response bit set
        if n >= 12 && buf[0..2] == id.to_be_bytes() && buf[2] & 0x80 != 0 {
            return Some(t0.elapsed().as_secs_f32() * 1000.0);
        }
    }
}

/// Response time of a DNS server: best of three queries (the first may include the resolver's
/// own lookup). None when it does not answer.
pub fn probe(server: IpAddr) -> Option<f32> {
    let base = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0x5157, |d| d.subsec_nanos() as u16);
    let mut best: Option<f32> = None;
    let mut misses = 0;
    for i in 0..3u16 {
        match probe_once(server, base.wrapping_add(i)) {
            Some(ms) => best = Some(best.map_or(ms, |b| b.min(ms))),
            None => misses += 1,
        }
        if misses == 2 {
            break;
        }
    }
    best
}

/// An address where no DNS server can exist (TEST-NET-1, RFC 5737). An answer from it means
/// something on the way catches all DNS traffic and answers itself.
pub const NO_SERVER: [u8; 4] = [192, 0, 2, 1];

/// What a measurement run found so far.
#[derive(Clone, Debug, Default)]
pub struct BenchResult {
    /// (server, ms or None = no answer), in the order measured
    pub times: Vec<(IpAddr, Option<f32>)>,
    /// a run is going
    pub running: bool,
    /// DNS on this network is intercepted (an address without a server answered); None = not
    /// checked yet
    pub intercepted: Option<bool>,
}

/// Response times measured in the background, one server after the other.
#[derive(Default)]
pub struct Bench {
    shared: std::sync::Arc<std::sync::Mutex<BenchResult>>,
    pub started: Option<std::time::Instant>,
}

impl Bench {
    /// Measure `servers` in order on a thread (nothing when a run is still going). Earlier
    /// results stay until their server is measured again.
    pub fn start(&mut self, servers: Vec<IpAddr>) {
        if self.snapshot().running || servers.is_empty() {
            return;
        }
        self.started = Some(std::time::Instant::now());
        let shared = self.shared.clone();
        if let Ok(mut g) = shared.lock() {
            g.running = true;
        }
        std::thread::spawn(move || {
            // first: does an address without any DNS server answer?
            let caught = probe_once(IpAddr::from(NO_SERVER), 0x7E57).is_some();
            if caught {
                crate::log!("dns: intercepted on this network ({} answered)", IpAddr::from(NO_SERVER));
            }
            if let Ok(mut g) = shared.lock() {
                g.intercepted = Some(caught);
            }
            for s in servers {
                let ms = probe(s);
                if let Ok(mut g) = shared.lock() {
                    g.times.retain(|(ip, _)| *ip != s);
                    g.times.push((s, ms));
                }
            }
            if let Ok(mut g) = shared.lock() {
                g.running = false;
            }
        });
    }

    /// What was found so far.
    pub fn snapshot(&self) -> BenchResult {
        self.shared.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// The DNS state of the interface with this alias.
pub fn read(alias: &str) -> Option<DnsState> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::NetworkManagement::IpHelper::{GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST, IP_ADAPTER_ADDRESSES_LH};
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6};
    if alias.is_empty() {
        return None;
    }
    unsafe {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_UNICAST;
        let mut size = 32 * 1024u32;
        let mut buf = vec![0u8; size as usize];
        let mut r = GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        if r == 111 {
            buf.resize(size as usize, 0);
            r = GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        }
        if r != ERROR_SUCCESS.0 {
            return None;
        }
        let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !p.is_null() {
            let a = &*p;
            if a.FriendlyName.to_string().unwrap_or_default() == alias {
                let mut servers = Vec::new();
                let mut d = a.FirstDnsServerAddress;
                while !d.is_null() {
                    let sa = (*d).Address.lpSockaddr;
                    if !sa.is_null() {
                        if (*sa).sa_family == AF_INET {
                            let b = (*(sa as *const SOCKADDR_IN)).sin_addr.S_un.S_un_b;
                            servers.push(IpAddr::from([b.s_b1, b.s_b2, b.s_b3, b.s_b4]));
                        } else if (*sa).sa_family == AF_INET6 {
                            let b = (*(sa as *const SOCKADDR_IN6)).sin6_addr.u.Byte;
                            let ip = std::net::Ipv6Addr::from(b);
                            // site-local fec0:: placeholders Windows lists when nothing is known
                            if ip.segments()[0] != 0xfec0 {
                                servers.push(IpAddr::V6(ip));
                            }
                        }
                    }
                    d = (*d).Next;
                }
                let guid = a.AdapterName.to_string().unwrap_or_default();
                let set_by_hand = |stack: &str| {
                    crate::util::reg_sz_hklm(&format!(r"SYSTEM\CurrentControlSet\Services\{stack}\Parameters\Interfaces\{guid}"), "NameServer").map_or(false, |v| !v.trim().is_empty())
                };
                let automatic = !set_by_hand("Tcpip") && !set_by_hand("Tcpip6");
                return Some(DnsState { alias: alias.to_string(), servers, automatic });
            }
            p = a.Next;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn presets_parse() {
        for p in PRESETS {
            assert!(p.servers.iter().all(|s| s.parse::<IpAddr>().is_ok()), "{}", p.name);
        }
    }

    #[test]
    fn choice_follows_the_ipv4_servers_in_any_order() {
        let st = DnsState { alias: "Wi-Fi".into(), servers: ips(&["1.0.0.1", "1.1.1.1", "2606:4700:4700::1111"]), automatic: false };
        assert_eq!(st.choice(), Choice::Preset(0));
        let st = DnsState { servers: ips(&["9.9.9.9", "149.112.112.112"]), ..st };
        assert_eq!(st.choice(), Choice::Preset(2));
        let st = DnsState { servers: ips(&["198.51.100.53"]), ..st };
        assert_eq!(st.choice(), Choice::Custom);
        let st = DnsState { automatic: true, ..st };
        assert_eq!(st.choice(), Choice::Automatic);
    }

    #[test]
    fn custom_servers_are_parsed_strictly() {
        assert_eq!(parse_servers("1.1.1.1, 9.9.9.9"), Some(ips(&["1.1.1.1", "9.9.9.9"])));
        assert_eq!(parse_servers("198.51.100.53;2001:db8::1"), Some(ips(&["198.51.100.53", "2001:db8::1"])));
        assert_eq!(parse_servers(""), None);
        assert_eq!(parse_servers("1.1.1.1 dns.google"), None);
        assert_eq!(parse_servers("1.1.1.1 1.1.1.2 1.1.1.3 1.1.1.4 1.1.1.5 1.1.1.6 1.1.1.7"), None);
    }

    #[test]
    fn commands_quote_the_alias_and_list_the_servers() {
        let c = command("Wi-Fi", Some(&ips(&["1.1.1.1", "2606:4700:4700::1111"]))).unwrap();
        assert!(c.contains("-InterfaceAlias 'Wi-Fi' -ServerAddresses @('1.1.1.1','2606:4700:4700::1111')"), "{c}");
        assert!(c.ends_with("Clear-DnsClientCache"));
        assert!(command("Wi-Fi", None).unwrap().contains("-ResetServerAddresses"));
        assert!(command("Bob's LAN", None).unwrap().contains("'Bob''s LAN'"));
        assert_eq!(command("bad\"name", None), None);
        assert_eq!(command("", None), None);
    }

    #[test]
    fn query_packet_is_a_standard_a_query() {
        let p = query_packet(0x1234, "example.com");
        assert_eq!(&p[..12], &[0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&p[12..], b"\x07example\x03com\x00\x00\x01\x00\x01");
    }

    /// `cargo test dns_speed_probe -- --ignored --nocapture`: sends real queries.
    #[test]
    #[ignore]
    fn dns_speed_probe() {
        for p in PRESETS {
            let ip: IpAddr = p.servers[0].parse().unwrap();
            let v: Vec<_> = (0..4u16).map(|i| probe_once(ip, 0x4000 + i)).collect();
            println!("{} {ip}: {:?}", p.name, v);
        }
    }

    /// `cargo test dns_probe -- --ignored --nocapture`: read-only.
    #[test]
    #[ignore]
    fn dns_probe() {
        let st = read("Wi-Fi");
        println!("{st:#?}");
        if let Some(s) = st {
            println!("choice: {:?}", s.choice());
        }
    }
}
