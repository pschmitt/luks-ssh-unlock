//! Watch DHCP traffic for the target's initrd and start a validated one-shot
//! unlock (`run --once` in a transient systemd unit) as soon as its SSH port
//! answers. Needs CAP_NET_RAW.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::io::{self, BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use clap::Args;

use crate::log::Logger;

#[derive(Args, Debug, Clone)]
pub struct DhcpArgs {
    /// Interface to listen on, or "any"
    #[arg(long)]
    pub interface: String,
    /// DHCP client hostname the target's initrd announces
    #[arg(long)]
    pub client_hostname: String,
    /// Target hostname (its address is also matched against requests)
    #[arg(long)]
    pub target_hostname: String,
    /// Seconds to wait for SSH after a lease
    #[arg(long, default_value_t = 180)]
    pub ssh_wait_timeout: u64,
    /// Environment file of the instance
    #[arg(long)]
    pub environment_file: PathBuf,
    /// Unlocker binary to run (default: this binary)
    #[arg(long)]
    pub unlocker: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DhcpPacket {
    pub op: u8,
    pub message_type: u8,
    pub xid: [u8; 4],
    pub chaddr: [u8; 6],
    pub hostname: String,
    pub yiaddr: Ipv4Addr,
    pub requested: Option<Ipv4Addr>,
}

/// Parse an IPv4/UDP datagram carrying BOOTP/DHCP.
pub fn parse(packet: &[u8]) -> Option<DhcpPacket> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(packet[0] & 0x0f) * 4;
    if packet[9] != 17 || packet.len() < ihl + 8 + 240 {
        return None;
    }
    let sport = u16::from_be_bytes([packet[ihl], packet[ihl + 1]]);
    let dport = u16::from_be_bytes([packet[ihl + 2], packet[ihl + 3]]);
    if !matches!((sport, dport), (67, 68) | (68, 67)) {
        return None;
    }
    let bootp = &packet[ihl + 8..];
    if bootp[236..240] != [0x63, 0x82, 0x53, 0x63] {
        return None;
    }
    let mut options: HashMap<u8, &[u8]> = HashMap::new();
    let mut off = 240;
    while off < bootp.len() {
        let code = bootp[off];
        off += 1;
        match code {
            255 => break,
            0 => continue,
            _ => {}
        }
        let Some(&len) = bootp.get(off) else { break };
        off += 1;
        let end = (off + usize::from(len)).min(bootp.len());
        options.insert(code, &bootp[off..end]);
        off = end;
    }
    let message_type = *options.get(&53)?.first()?;
    let ip = |b: &[u8]| Ipv4Addr::new(b[0], b[1], b[2], b[3]);
    Some(DhcpPacket {
        op: bootp[0],
        message_type,
        xid: bootp[4..8].try_into().ok()?,
        chaddr: bootp[28..34].try_into().ok()?,
        hostname: options
            .get(&12)
            .map(|h| {
                String::from_utf8_lossy(h)
                    .trim_end_matches('.')
                    .to_ascii_lowercase()
            })
            .unwrap_or_default(),
        yiaddr: ip(&bootp[16..20]),
        requested: options.get(&50).filter(|v| v.len() == 4).map(|v| ip(v)),
    })
}

fn interfaces() -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: if_nameindex returns a NULL-terminated array we free below.
    unsafe {
        let list = libc::if_nameindex();
        if list.is_null() {
            return out;
        }
        let mut p = list;
        while (*p).if_index != 0 && !(*p).if_name.is_null() {
            out.push(CStr::from_ptr((*p).if_name).to_string_lossy().into_owned());
            p = p.add(1);
        }
        libc::if_freenameindex(list);
    }
    out
}

fn open_socket(interface: &str) -> io::Result<OwnedFd> {
    let name = CString::new(interface).map_err(io::Error::other)?;
    let proto = (libc::ETH_P_IP as u16).to_be();
    // SAFETY: plain socket syscalls on owned descriptors and zeroed structs.
    unsafe {
        let index = libc::if_nametoindex(name.as_ptr());
        if index == 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            i32::from(proto),
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_ll = std::mem::zeroed();
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = proto;
        addr.sll_ifindex = index as i32;
        let rc = libc::bind(
            fd.as_raw_fd(),
            std::ptr::addr_of!(addr).cast(),
            std::mem::size_of::<libc::sockaddr_ll>() as u32,
        );
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn wait_for_ssh(address: Ipv4Addr, port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let addr = SocketAddr::from((address, port));
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_secs(2));
    }
    false
}

#[derive(Default)]
struct Tracker {
    active: HashSet<Ipv4Addr>,
    recent: HashMap<Ipv4Addr, Instant>,
}

struct Listener {
    args: DhcpArgs,
    ssh_port: u16,
    unlocker: PathBuf,
    log: Logger,
    tracker: Arc<Mutex<Tracker>>,
}

impl Listener {
    fn handle(&self, address: Ipv4Addr) {
        let log = &self.log;
        let a = &self.args;
        log.info(format!(
            "Waiting for SSH at {address} for {}",
            a.client_hostname
        ));
        if !wait_for_ssh(
            address,
            self.ssh_port,
            Duration::from_secs(a.ssh_wait_timeout),
        ) {
            log.warn(format!(
                "SSH did not come up at {address} within {}s",
                a.ssh_wait_timeout
            ));
            return;
        }
        log.info(format!(
            "SSH is up at {address}; starting a validated unlock of {}",
            a.target_hostname
        ));
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--system", "--wait", "--pipe", "--collect", "--quiet"])
            .arg(format!(
                "--unit=luks-ssh-unlock-dhcp-{}-{stamp}",
                a.client_hostname
            ))
            .arg(format!(
                "--property=EnvironmentFile={}",
                a.environment_file.display()
            ))
            .arg(format!("--setenv=SSH_CONNECT_ADDRESS={address}"))
            .arg(format!("--setenv=SSH_HOSTKEY_ALIAS={}", a.target_hostname));
        if let Some(dir) = std::env::var_os("CREDENTIALS_DIRECTORY") {
            let credential = PathBuf::from(dir).join("luks-passphrase");
            if credential.is_file() {
                cmd.arg(format!(
                    "--property=LoadCredential=luks-passphrase:{}",
                    credential.display()
                ));
            }
        }
        cmd.arg("--").arg(&self.unlocker).args(["run", "--once"]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                log.error(format!("Cannot run systemd-run: {e}"));
                return;
            }
        };
        let err_log = log.clone();
        let stderr = child.stderr.take().map(|s| {
            thread::spawn(move || {
                for line in BufReader::new(s).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        err_log.info(format!("unlocker: {line}"));
                    }
                }
            })
        });
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    log.info(format!("unlocker: {line}"));
                }
            }
        }
        if let Some(h) = stderr {
            let _ = h.join();
        }
        match child.wait() {
            Ok(s) if s.success() => log.info(format!(
                "Validated unlock attempt for {} completed",
                a.target_hostname
            )),
            Ok(s) => log.error(format!(
                "Unlock attempt for {} failed ({s})",
                a.target_hostname
            )),
            Err(e) => log.error(format!(
                "Unlock attempt for {} failed: {e}",
                a.target_hostname
            )),
        }
    }

    /// Start an unlock for ADDRESS unless one runs or ran within a minute.
    fn start(self: &Arc<Self>, address: Ipv4Addr) -> bool {
        {
            let mut t = self.tracker.lock().expect("tracker lock");
            let now = Instant::now();
            if t.active.contains(&address)
                || t.recent
                    .get(&address)
                    .is_some_and(|at| now.duration_since(*at) < Duration::from_secs(60))
            {
                return false;
            }
            t.active.insert(address);
            t.recent.insert(address, now);
        }
        let me = Arc::clone(self);
        thread::spawn(move || {
            me.handle(address);
            me.tracker
                .lock()
                .expect("tracker lock")
                .active
                .remove(&address);
        });
        true
    }
}

/// SSH_PORT is the global `--port`/`--ssh-port` option.
pub fn run(args: DhcpArgs, ssh_port: u16, log: Logger) -> Result<()> {
    let unlocker = match &args.unlocker {
        Some(p) => p.clone(),
        None => std::env::current_exe()?,
    };
    log.info(format!(
        "Listening for DHCP traffic of {} on {}",
        args.client_hostname, args.interface
    ));
    let targets: HashSet<Ipv4Addr> = (args.target_hostname.as_str(), 0)
        .to_socket_addrs()
        .map(|it| {
            it.filter_map(|a| match a {
                SocketAddr::V4(v4) => Some(*v4.ip()),
                SocketAddr::V6(_) => None,
            })
            .collect()
        })
        .unwrap_or_else(|_| {
            log.warn(format!("Cannot resolve {}", args.target_hostname));
            HashSet::new()
        });
    let client = args.client_hostname.to_ascii_lowercase();
    let any = args.interface == "any";
    let listener = Arc::new(Listener {
        args: args.clone(),
        ssh_port,
        unlocker,
        log: log.clone(),
        tracker: Arc::default(),
    });

    let mut sockets: Vec<(String, OwnedFd)> = Vec::new();
    let bind = |sockets: &mut Vec<(String, OwnedFd)>| {
        // With "any", interfaces come and go (container veths, VPN links).
        let names = if any {
            interfaces()
        } else {
            vec![args.interface.clone()]
        };
        for name in names {
            if sockets.iter().any(|(n, _)| *n == name) {
                continue;
            }
            match open_socket(&name) {
                Ok(fd) => sockets.push((name, fd)),
                Err(e) => log.warn(format!("Cannot listen on {name}: {e}")),
            }
        }
    };
    bind(&mut sockets);
    if sockets.is_empty() {
        bail!("cannot listen for DHCP on any interface (CAP_NET_RAW missing?)");
    }

    let mut pending: HashMap<[u8; 6], ([u8; 4], Instant)> = HashMap::new();
    let mut last_bind = Instant::now();
    let mut buf = [0u8; 4096];
    loop {
        if any && last_bind.elapsed() >= Duration::from_secs(30) {
            bind(&mut sockets);
            last_bind = Instant::now();
        }
        let mut fds: Vec<libc::pollfd> = sockets
            .iter()
            .map(|(_, fd)| libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: fds is a valid, correctly sized pollfd array.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 30_000) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        let mut dead = Vec::new();
        for (i, pfd) in fds.iter().enumerate() {
            if pfd.revents == 0 {
                continue;
            }
            // SAFETY: reading into a stack buffer of the given size.
            let len = unsafe {
                libc::recv(
                    pfd.fd,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if len < 0 {
                let e = io::Error::last_os_error();
                // ENETDOWN: the link went down; the socket recovers with it.
                if !matches!(
                    e.raw_os_error(),
                    Some(libc::ENETDOWN | libc::EAGAIN | libc::EINTR)
                ) {
                    log.warn(format!("Dropping listener on {}: {e}", sockets[i].0));
                    dead.push(i);
                }
                continue;
            }
            let Some(p) = parse(&buf[..len as usize]) else {
                continue;
            };

            if p.op == 1 && matches!(p.message_type, 1 | 3) {
                let requested_target = p.requested.is_some_and(|r| targets.contains(&r));
                if p.hostname != client && !requested_target {
                    continue;
                }
                if p.message_type == 3 && requested_target {
                    pending.remove(&p.chaddr);
                    let addr = p.requested.expect("checked");
                    if listener.start(addr) {
                        log.info(format!(
                            "DHCP request from {client} asks for {addr}; waiting for SSH"
                        ));
                    }
                    continue;
                }
                if pending.get(&p.chaddr).is_some_and(|(xid, _)| *xid == p.xid) {
                    continue;
                }
                pending.insert(p.chaddr, (p.xid, Instant::now()));
                log.info(format!(
                    "Detected a DHCP request from {client}; waiting for its lease"
                ));
                continue;
            }
            if p.op != 2 || p.message_type != 5 {
                continue;
            }
            if pending
                .get(&p.chaddr)
                .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(180))
            {
                pending.remove(&p.chaddr);
            }
            let was_pending = pending.remove(&p.chaddr).is_some();
            if p.hostname != client && !was_pending {
                continue;
            }
            if listener.start(p.yiaddr) {
                log.info(format!(
                    "DHCP assigned {} to {client}; waiting for SSH",
                    p.yiaddr
                ));
            }
        }
        for i in dead.into_iter().rev() {
            sockets.remove(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(
        op: u8,
        msg: u8,
        hostname: Option<&str>,
        yiaddr: [u8; 4],
        requested: Option<[u8; 4]>,
    ) -> Vec<u8> {
        let mut bootp = vec![0u8; 240];
        bootp[0] = op;
        bootp[4..8].copy_from_slice(&[1, 2, 3, 4]);
        bootp[16..20].copy_from_slice(&yiaddr);
        bootp[28..34].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        bootp[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        bootp.extend([53, 1, msg]);
        if let Some(h) = hostname {
            bootp.extend([12, h.len() as u8]);
            bootp.extend(h.as_bytes());
        }
        if let Some(r) = requested {
            bootp.extend([50, 4]);
            bootp.extend(r);
        }
        bootp.push(255);
        let (sport, dport) = if op == 1 { (68u16, 67u16) } else { (67, 68) };
        let mut ip = vec![
            0x45, 0, 0, 0, 0, 0, 0, 0, 64, 17, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255,
        ];
        ip.extend(sport.to_be_bytes());
        ip.extend(dport.to_be_bytes());
        ip.extend([0, 0, 0, 0]);
        ip.extend(bootp);
        ip
    }

    #[test]
    fn parses_request_and_ack() {
        let req = parse(&packet(1, 3, Some("Target."), [0; 4], Some([10, 0, 0, 5]))).unwrap();
        assert_eq!(req.op, 1);
        assert_eq!(req.message_type, 3);
        assert_eq!(req.hostname, "target");
        assert_eq!(req.requested, Some(Ipv4Addr::new(10, 0, 0, 5)));
        assert_eq!(req.chaddr, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);

        let ack = parse(&packet(2, 5, None, [10, 0, 0, 6], None)).unwrap();
        assert_eq!(ack.yiaddr, Ipv4Addr::new(10, 0, 0, 6));
        assert_eq!(ack.hostname, "");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(&[0u8; 10]).is_none());
        let mut p = packet(1, 3, None, [0; 4], None);
        p[9] = 6; // TCP
        assert!(parse(&p).is_none());
        let mut p = packet(1, 3, None, [0; 4], None);
        let cookie = 20 + 8 + 236;
        p[cookie] = 0;
        assert!(parse(&p).is_none());
    }
}
