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
//! NetworkManager's own scans turn up; it never scans by itself.

use crate::nm::{Better, Nm, Outcome, NM, NM_PATH, WIRELESS};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use zbus::blocking::{fdo::DBusProxy, MessageIterator};
use zbus::message::Type;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

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
    let Ok(dbus) = DBusProxy::new(nm.bus()) else {
        eprintln!("roam: cannot reach the bus itself");
        std::process::exit(1);
    };
    // The stream first, so no signal falls between the rule and it.
    let stream = MessageIterator::from(nm.bus());
    if dbus.add_match_rule(changed).is_err() {
        eprintln!("roam: the bus refused to pass on NetworkManager's changes");
        std::process::exit(1);
    }

    let mut w = Watch { nm, dbus, added, metered: false, better: HashMap::new(), rc: read_rc() };
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
}

impl Watch {
    /// Listen for new access points only while on a metered network.
    fn sync(&mut self) {
        let now = self.nm.on_metered();
        if now == self.metered {
            return;
        }
        self.metered = now;
        if now {
            self.better = self.nm.better(&portals());
            self.rc = read_rc();
            let _ = self.dbus.add_match_rule(self.added.clone());
            eprintln!("roam: on a metered network; {} saved networks would do better", self.better.len());
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
