//! `roam --watch`: leave a metered network when a better saved one comes
//! into range, and bring back the VPN that was up.
//!
//! "Better" is a saved network with a password (so no captive portal),
//! set to join by itself, and not metered. Nothing is named in the code:
//! the metered mark lives in NetworkManager (`m` in roam sets it), the VPN
//! commands in `~/.roamrc`.
//!
//! It sleeps on D-Bus signals. On an unmetered network it hears only
//! NetworkManager's own properties changing, which they do when the
//! connection changes. On a metered one it also hears each access point
//! a scan turns up, and asks for a scan every three minutes:
//! NetworkManager looks by itself only when the signal gets weak, and a
//! phone's hotspot in a pocket never does.
//!
//! While on a metered network it also counts the data that goes through
//! the Wi-Fi card, and writes the sum down at each of those looks:
//! `~/.roam/usage`, a line a day and network. roam shows it in its top bar.

use crate::nm::{Better, Nm, Outcome, NM, NM_PATH, WIRELESS};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::blocking::{fdo::DBusProxy, MessageIterator};
use zbus::message::Type;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

/// How often to look for a better network while on a metered one.
const LOOK_EVERY: Duration = Duration::from_secs(180);

/// What `~/.roamrc` says to run for a VPN NetworkManager does not know.
#[derive(Default)]
struct Rc {
    vpn_up: Option<String>,
    vpn_down: Option<String>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// `key = value` lines, `#` for comments.
fn read_rc() -> Rc {
    let mut rc = Rc::default();
    let text = fs::read_to_string(home().join(".roamrc")).unwrap_or_default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "vpn_up" => rc.vpn_up = Some(v.to_string()),
            "vpn_down" => rc.vpn_down = Some(v.to_string()),
            _ => {}
        }
    }
    rc
}

/// Networks that turned out to be captive portals, one name a line.
fn portals_path() -> PathBuf {
    home().join(".roam").join("portals")
}

fn portals() -> Vec<String> {
    fs::read_to_string(portals_path()).unwrap_or_default().lines().map(str::to_string).collect()
}

fn remember_portal(ssid: &str) {
    let mut all = portals();
    all.push(ssid.to_string());
    let _ = fs::create_dir_all(home().join(".roam"));
    let _ = fs::write(portals_path(), all.join("\n") + "\n");
}

/// Where the kernel keeps each network card's counters.
const SYS: &str = "/sys/class/net";

/// The data used on metered networks: `2026-10-09<TAB>bytes<TAB>name`,
/// a line a day and network.
fn usage_path() -> PathBuf {
    home().join(".roam").join("usage")
}

/// The day as 2026-10-09, by the clock on the wall.
fn today() -> String {
    // SAFETY: time takes a null pointer, and localtime_r fills the tm
    // it is handed.
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// Bytes in and out through a network card since it came up.
fn counter(sys: &Path, iface: &str) -> Option<u64> {
    let read = |f: &str| fs::read_to_string(sys.join(iface).join("statistics").join(f)).ok()?.trim().parse::<u64>().ok();
    Some(read("rx_bytes")? + read("tx_bytes")?)
}

/// One line of the usage file: the day, the bytes, the network's name.
fn fields(line: &str) -> Option<(&str, u64, &str)> {
    let mut f = line.splitn(3, '\t');
    Some((f.next()?, f.next()?.parse().ok()?, f.next()?))
}

/// The usage file with `bytes` more on `day` for the network `name`.
/// Its line moves to the end, so the last line is the one counted last.
fn add_use(text: &str, day: &str, name: &str, bytes: u64) -> String {
    let mut sum = bytes;
    let mut out = String::new();
    for line in text.lines() {
        match fields(line) {
            Some((d, b, n)) if d == day && n == name => sum += b,
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out.push_str(&format!("{day}\t{sum}\t{name}\n"));
    out
}

/// The network counted last, with its bytes on `day` and in that month.
fn used(text: &str, day: &str) -> Option<(String, u64, u64)> {
    let name = fields(text.lines().last()?)?.2;
    let month = day.get(..7)?;
    let (mut that_day, mut that_month) = (0, 0);
    for (d, b, n) in text.lines().filter_map(fields) {
        if n != name || !d.starts_with(month) {
            continue;
        }
        that_month += b;
        if d == day {
            that_day += b;
        }
    }
    (that_month > 0).then(|| (name.to_string(), that_day, that_month))
}

/// For roam's top bar: the metered network counted last, with its data
/// today and this month. Nothing while the month has no count.
pub fn used_now() -> Option<(String, u64, u64)> {
    used(&fs::read_to_string(usage_path()).ok()?, &today())
}

/// The count of one metered network's data: which network, through
/// which card, and what the card's counter said when it was last read.
struct Meter {
    name: String,
    iface: String,
    last: u64,
}

impl Meter {
    /// Write down what went through the card since the last time.
    fn tally(&mut self, sys: &Path, file: &Path, day: &str) {
        let Some(now) = counter(sys, &self.iface) else { return };
        // A counter that went back started again from nothing.
        let more = if now >= self.last { now - self.last } else { now };
        self.last = now;
        if more == 0 {
            return;
        }
        let text = add_use(&fs::read_to_string(file).unwrap_or_default(), day, &self.name, more);
        if let Some(dir) = file.parent() {
            let _ = fs::create_dir_all(dir);
        }
        // Whole or not at all: roam may read it at any moment.
        let tmp = file.with_extension("new");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, file);
        }
    }
}

/// A tunnel NetworkManager does not run, such as openfortivpn's: a ppp
/// or tun device that is up.
fn tunnel_up() -> bool {
    let Ok(dir) = fs::read_dir("/sys/class/net") else { return false };
    dir.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        (name.starts_with("ppp") || name.starts_with("tun"))
            && fs::read_to_string(e.path().join("operstate")).is_ok_and(|s| s.trim() != "down")
    })
}

/// A line in the corner of the screen, through whatever notifier runs.
fn tell(body: &str) {
    let Ok(bus) = zbus::blocking::Connection::session() else { return };
    let hints: HashMap<&str, Value> = HashMap::new();
    let actions: Vec<&str> = Vec::new();
    let _ = bus.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &("roam", 0u32, "", "roam", body, actions, hints, 8000i32),
    );
}

pub fn run() {
    // One watcher only: a second would switch networks under the first.
    // The name is freed by the kernel when the process ends, however it ends.
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    let Ok(name) = SocketAddr::from_abstract_name(b"roam-watch") else { return };
    let Ok(_one) = UnixListener::bind_addr(&name) else {
        eprintln!("roam: already watching");
        return;
    };
    let nm = match Nm::new() {
        Ok(nm) => nm,
        Err(e) => {
            eprintln!("roam: cannot reach NetworkManager on the system bus: {e}");
            std::process::exit(1);
        }
    };
    let Some(wifi) = nm.wifi_path().map(str::to_string) else {
        eprintln!("roam: no Wi-Fi card to watch");
        std::process::exit(1);
    };
    let rule = |path: &str, iface: &str, member: &str| -> zbus::Result<MatchRule<'static>> {
        Ok(MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(NM)?
            .path(path.to_string())?
            .interface(iface.to_string())?
            .member(member.to_string())?
            .build())
    };
    let (Ok(changed), Ok(added)) = (
        rule(NM_PATH, "org.freedesktop.DBus.Properties", "PropertiesChanged"),
        rule(&wifi, WIRELESS, "AccessPointAdded"),
    ) else {
        eprintln!("roam: bad match rule");
        std::process::exit(1);
    };
    // The signals come in on a connection of their own. On the one the
    // calls use, every reply is queued for the listener as well, and a
    // burst of calls (the saved networks, read one by one) filled that
    // queue while the listener itself waited for a reply: nothing moved
    // again, and roam slept through the move home.
    let Ok(signals) = zbus::blocking::Connection::system() else {
        eprintln!("roam: cannot reach the system bus");
        std::process::exit(1);
    };
    let Ok(dbus) = DBusProxy::new(&signals) else {
        eprintln!("roam: cannot reach the bus itself");
        std::process::exit(1);
    };
    // The stream first, so no signal falls between the rule and it.
    let stream = MessageIterator::from(&signals);
    if dbus.add_match_rule(changed).is_err() {
        eprintln!("roam: the bus refused to pass on NetworkManager's changes");
        std::process::exit(1);
    }

    let mut w = Watch { nm, dbus, added, metered: false, better: HashMap::new(), rc: read_rc(), looking: Arc::default(), meter: Arc::default() };
    w.sync();
    for msg in stream {
        let Ok(msg) = msg else { continue };
        let h = msg.header();
        if h.message_type() != Type::Signal {
            continue;
        }
        match h.member().map(|m| m.as_str()) {
            Some("PropertiesChanged") if h.path().is_some_and(|p| p.as_str() == NM_PATH) => {
                let body: zbus::Result<(String, HashMap<String, OwnedValue>, Vec<String>)> = msg.body().deserialize();
                if body.is_ok_and(|(_, props, _)| props.contains_key("Metered")) {
                    w.sync();
                }
            }
            Some("AccessPointAdded") if w.metered => {
                let Ok(ap) = msg.body().deserialize::<OwnedObjectPath>() else { continue };
                if w.better.contains_key(&w.nm.ap_ssid(ap.as_str())) {
                    w.switch();
                    w.sync();
                }
            }
            _ => {}
        }
    }
}

struct Watch {
    nm: Nm,
    dbus: DBusProxy<'static>,
    added: MatchRule<'static>,
    metered: bool,
    /// While metered: the saved networks worth switching to, by name.
    better: HashMap<String, Better>,
    rc: Rc,
    /// Bumped on every change of metered: a scanning thread that sees a
    /// number other than its own stops.
    looking: Arc<AtomicU64>,
    /// While on a metered Wi-Fi network: the count of its data.
    meter: Arc<Mutex<Option<Meter>>>,
}

impl Watch {
    /// Listen for new access points only while on a metered network.
    fn sync(&mut self) {
        let now = self.nm.on_metered();
        if now == self.metered {
            return;
        }
        self.metered = now;
        let round = self.looking.fetch_add(1, Ordering::Relaxed) + 1;
        // What went through up to now belongs to the network just left.
        if let Ok(mut m) = self.meter.lock() {
            if let Some(mut old) = m.take() {
                old.tally(Path::new(SYS), &usage_path(), &today());
            }
            if now {
                let iface = self.nm.wifi_iface().to_string();
                *m = self.nm.wifi_now().and_then(|(_, _, name)| {
                    let last = counter(Path::new(SYS), &iface)?;
                    Some(Meter { name: name.replace(['\t', '\n'], " "), iface, last })
                });
            }
        }
        if now {
            let (nm, looking, meter) = (self.nm.clone(), self.looking.clone(), self.meter.clone());
            std::thread::spawn(move || loop {
                std::thread::sleep(LOOK_EVERY);
                if looking.load(Ordering::Relaxed) != round {
                    break;
                }
                if let Some(m) = meter.lock().ok().as_mut().and_then(|m| m.as_mut()) {
                    m.tally(Path::new(SYS), &usage_path(), &today());
                }
                nm.scan();
            });
            self.better = self.nm.better(&portals());
            self.rc = read_rc();
            let _ = self.dbus.add_match_rule(self.added.clone());
            eprintln!("roam: on a metered network; {} saved networks would do better", self.better.len());
            // One may be in range already: seen while the hotspot dropped
            // out, it is no longer new when a scan finds it again.
            if self.best_in_air().is_some() {
                self.switch();
            }
        } else {
            self.better.clear();
            let _ = self.dbus.remove_match_rule(self.added.clone());
            eprintln!("roam: off metered; idle");
        }
    }

    /// Move to the best network in range, and bring back what was up.
    fn switch(&mut self) {
        let Some((to, ap)) = self.best_in_air() else { return };
        eprintln!("roam: {} in range", to.ssid);
        let before = self.nm.wifi_now();
        let from = before.as_ref().map(|b| b.2.clone()).unwrap_or_default();
        let vpns = self.nm.vpns_up();
        // A tunnel of its own is openfortivpn's; one under a VPN
        // NetworkManager runs is that VPN's.
        let tunnel = vpns.is_empty() && tunnel_up() && self.rc.vpn_up.is_some();
        if tunnel {
            if let Some(cmd) = &self.rc.vpn_down {
                let _ = Command::new("sh").arg("-c").arg(cmd).status();
            }
        }
        let up = match self.nm.activate(&to.conn, Some(&ap)) {
            Ok(a) => matches!(self.nm.wait(&a, Duration::from_secs(25)), Outcome::Up),
            Err(_) => false,
        };
        let portal = up && self.nm.behind_portal();
        if portal {
            remember_portal(&to.ssid);
        }
        if up && !portal {
            let vpn = if tunnel || !vpns.is_empty() { "; bringing the VPN back" } else { "" };
            tell(&format!("Left {from} for {}{vpn}", to.ssid));
        } else {
            // Back where it was, VPN and all.
            if let Some((_, conn, _)) = &before {
                if let Ok(a) = self.nm.activate(conn, None) {
                    self.nm.wait(&a, Duration::from_secs(25));
                }
            }
            let why = if portal { "a login page; it is skipped from now on" } else { "no way in" };
            tell(&format!("{} had {why}; back on {from}", to.ssid));
        }
        for v in &vpns {
            if let Ok(a) = self.nm.vpn_up(v) {
                self.nm.wait(&a, Duration::from_secs(30));
            }
        }
        if tunnel {
            if let Some(cmd) = &self.rc.vpn_up {
                // It may stay running (a sign-in window), so it is left to
                // run, and reaped when it ends.
                if let Ok(mut child) = Command::new("sh").arg("-c").arg(cmd).spawn() {
                    std::thread::spawn(move || child.wait());
                }
            }
        }
    }

    /// The better network in range that NetworkManager would pick first:
    /// highest priority, then the one used last, then the strongest.
    fn best_in_air(&self) -> Option<(Better, OwnedObjectPath)> {
        self.nm
            .in_air()
            .into_iter()
            .filter_map(|(ssid, ap, strength)| self.better.get(&ssid).map(|b| (b.clone(), ap, strength)))
            .max_by_key(|(b, _, strength)| (b.priority, b.stamp, *strength))
            .map(|(b, ap, _)| (b, ap))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_days_data_adds_up_by_network() {
        let mut t = add_use("", "2026-09-30", "Phone", 700);
        t = add_use(&t, "2026-10-08", "Phone", 1000);
        t = add_use(&t, "2026-10-09", "Cabin 4G", 50);
        t = add_use(&t, "2026-10-09", "Phone", 300);
        t = add_use(&t, "2026-10-09", "Phone", 200);
        assert_eq!(t, "2026-09-30\t700\tPhone\n2026-10-08\t1000\tPhone\n2026-10-09\t50\tCabin 4G\n2026-10-09\t500\tPhone\n");
        // The network counted last, today and this month; September is out.
        assert_eq!(used(&t, "2026-10-09"), Some(("Phone".to_string(), 500, 1500)));
        assert_eq!(used(&t, "2026-10-10"), Some(("Phone".to_string(), 0, 1500)));
        assert_eq!(used(&t, "2026-11-01"), None);
        assert_eq!(used("", "2026-10-09"), None);
    }

    #[test]
    fn the_count_follows_the_cards_counter() {
        let dir = std::env::temp_dir().join(format!("roam-test-{}", std::process::id()));
        let stats = dir.join("wl0").join("statistics");
        fs::create_dir_all(&stats).unwrap();
        let set = |rx: u64, tx: u64| {
            fs::write(stats.join("rx_bytes"), format!("{rx}\n")).unwrap();
            fs::write(stats.join("tx_bytes"), format!("{tx}\n")).unwrap();
        };
        let file = dir.join("home").join("usage");
        set(900, 100);
        let mut m = Meter { name: "Phone".into(), iface: "wl0".into(), last: counter(&dir, "wl0").unwrap() };
        m.tally(&dir, &file, "2026-10-09");
        assert!(!file.exists(), "nothing went through, so nothing is written");
        set(4900, 1100);
        m.tally(&dir, &file, "2026-10-09");
        assert_eq!(fs::read_to_string(&file).unwrap(), "2026-10-09\t5000\tPhone\n");
        // The card started over: what it shows now is all new.
        set(100, 50);
        m.tally(&dir, &file, "2026-10-09");
        assert_eq!(fs::read_to_string(&file).unwrap(), "2026-10-09\t5150\tPhone\n");
        // A card that is gone changes nothing.
        m.iface = "gone".into();
        m.tally(&dir, &file, "2026-10-09");
        assert_eq!(fs::read_to_string(&file).unwrap(), "2026-10-09\t5150\tPhone\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
