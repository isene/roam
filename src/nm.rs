//! NetworkManager over D-Bus: what the air holds, what is saved, and the
//! few things roam asks of it.
//!
//! Everything goes over the system bus, the way nm-applet talks. No
//! nmcli is spawned, and a password travels inside one D-Bus call, so
//! it never lands in a process list or on disk.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const IP4: &str = "org.freedesktop.NetworkManager.IP4Config";

/// NetworkManager's number for a Wi-Fi device.
const DEVICE_WIFI: u32 = 2;

/// How a network is locked, as the access point says.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lock {
    Open,
    /// WPA or WPA2 with a password.
    Psk,
    /// WPA3 with a password, and nothing older on offer.
    Sae,
    /// A user name and a certificate: work networks, eduroam.
    Enterprise,
}

impl Lock {
    pub fn label(self) -> &'static str {
        match self {
            Lock::Open => "open",
            Lock::Psk => "WPA2",
            Lock::Sae => "WPA3",
            Lock::Enterprise => "802.1X",
        }
    }
}

/// One network in the air, with what NetworkManager knows about it.
#[derive(Clone)]
pub struct Net {
    pub ssid: String,
    pub strength: u8,
    pub lock: Lock,
    pub ap: OwnedObjectPath,
    /// The saved connection for it, when there is one.
    pub saved: Option<OwnedObjectPath>,
    /// The live connection, when this is the one in use.
    pub active: Option<OwnedObjectPath>,
}

/// A VPN NetworkManager knows how to bring up.
#[derive(Clone)]
pub struct Vpn {
    pub name: String,
    pub conn: OwnedObjectPath,
    pub active: Option<OwnedObjectPath>,
}

/// Everything roam shows, read in one go.
pub struct Look {
    pub wifi_on: bool,
    pub nets: Vec<Net>,
    pub vpns: Vec<Vpn>,
    /// What the machine is on right now, and its address, in words.
    pub now: String,
}

/// How an attempt to connect ended.
pub enum Outcome {
    Up,
    Failed,
    /// Still trying when roam stopped waiting.
    Slow,
}

pub struct Nm {
    bus: Connection,
    wifi: Option<OwnedObjectPath>,
}

fn root() -> OwnedObjectPath {
    OwnedObjectPath::try_from("/").expect("/ is a path")
}

impl Nm {
    pub fn new() -> zbus::Result<Nm> {
        let bus = Connection::system()?;
        let mut nm = Nm { bus, wifi: None };
        nm.wifi = nm.find_wifi();
        Ok(nm)
    }

    fn proxy<'a>(&'a self, path: &'a str, iface: &'a str) -> Option<Proxy<'a>> {
        Proxy::new(&self.bus, NM, path, iface).ok()
    }

    fn find_wifi(&self) -> Option<OwnedObjectPath> {
        let nm = self.proxy(NM_PATH, NM)?;
        let devices: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).ok()?;
        devices.into_iter().find(|d| {
            self.proxy(d.as_str(), DEVICE)
                .and_then(|p| p.get_property::<u32>("DeviceType").ok())
                == Some(DEVICE_WIFI)
        })
    }

    pub fn has_wifi(&self) -> bool {
        self.wifi.is_some()
    }

    /// Read everything: the networks in the air, the saved connections,
    /// what is live.
    pub fn look(&self) -> Look {
        let wifi_on = self
            .proxy(NM_PATH, NM)
            .and_then(|p| p.get_property::<bool>("WirelessEnabled").ok())
            .unwrap_or(false);

        // What is live, keyed by the saved connection it came from.
        let mut live: HashMap<String, OwnedObjectPath> = HashMap::new();
        if let Some(nm) = self.proxy(NM_PATH, NM) {
            let actives: Vec<OwnedObjectPath> = nm.get_property("ActiveConnections").unwrap_or_default();
            for a in actives {
                if let Some(c) = self.proxy(a.as_str(), ACTIVE)
                    .and_then(|p| p.get_property::<OwnedObjectPath>("Connection").ok())
                {
                    live.insert(c.as_str().to_string(), a);
                }
            }
        }

        // What is saved: Wi-Fi by its network name, VPNs by theirs.
        let mut saved: HashMap<String, OwnedObjectPath> = HashMap::new();
        let mut vpns = Vec::new();
        if let Some(settings) = self.proxy(SETTINGS_PATH, SETTINGS) {
            let conns: Vec<OwnedObjectPath> = settings.call("ListConnections", &()).unwrap_or_default();
            for c in conns {
                let Some(p) = self.proxy(c.as_str(), CONNECTION) else { continue };
                let Ok(s): Result<HashMap<String, HashMap<String, OwnedValue>>, _> = p.call("GetSettings", &()) else {
                    continue;
                };
                let kind = text(&s, "connection", "type");
                let id = text(&s, "connection", "id");
                match kind.as_str() {
                    "802-11-wireless" => {
                        let ssid = s.get("802-11-wireless")
                            .and_then(|w| w.get("ssid"))
                            .and_then(|v| Vec::<u8>::try_from(v.clone()).ok())
                            .map(|b| String::from_utf8_lossy(&b).to_string())
                            .unwrap_or_default();
                        // A live one wins over a spare copy of the same
                        // network ("Dualog 1").
                        let keep = !saved.contains_key(&ssid) || live.contains_key(c.as_str());
                        if keep {
                            saved.insert(ssid, c.clone());
                        }
                    }
                    "vpn" | "wireguard" => {
                        let active = live.get(c.as_str()).cloned();
                        vpns.push(Vpn { name: id, conn: c.clone(), active });
                    }
                    _ => {}
                }
            }
        }
        vpns.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        // What is in the air: the strongest access point per network name.
        let mut nets: Vec<Net> = Vec::new();
        if let Some(w) = self.wifi.as_ref().and_then(|w| self.proxy(w.as_str(), WIRELESS)) {
            let aps: Vec<OwnedObjectPath> = w.call("GetAllAccessPoints", &()).unwrap_or_default();
            for ap in aps {
                let Some(p) = self.proxy(ap.as_str(), AP) else { continue };
                let raw: Vec<u8> = p.get_property("Ssid").unwrap_or_default();
                let ssid = String::from_utf8_lossy(&raw).trim_end_matches('\0').to_string();
                // A hidden network has no name to show or to pick.
                if ssid.is_empty() {
                    continue;
                }
                let strength: u8 = p.get_property("Strength").unwrap_or(0);
                let lock = lock_of(
                    p.get_property("Flags").unwrap_or(0),
                    p.get_property("WpaFlags").unwrap_or(0),
                    p.get_property("RsnFlags").unwrap_or(0),
                );
                if let Some(n) = nets.iter_mut().find(|n| n.ssid == ssid) {
                    if strength > n.strength {
                        n.strength = strength;
                        n.ap = ap.clone();
                    }
                    continue;
                }
                let saved_conn = saved.get(&ssid).cloned();
                let active = saved_conn.as_ref().and_then(|c| live.get(c.as_str()).cloned());
                nets.push(Net { ssid, strength, lock, ap: ap.clone(), saved: saved_conn, active });
            }
        }
        // The live one first, then the ones you have used, then the rest,
        // each by signal.
        nets.sort_by(|a, b| {
            (b.active.is_some(), b.saved.is_some(), b.strength)
                .cmp(&(a.active.is_some(), a.saved.is_some(), a.strength))
        });

        Look { wifi_on, nets, vpns, now: self.now() }
    }

    /// What the machine is on, and the address it got.
    fn now(&self) -> String {
        let Some(nm) = self.proxy(NM_PATH, NM) else { return "no NetworkManager".into() };
        let primary: OwnedObjectPath = match nm.get_property("PrimaryConnection") {
            Ok(p) => p,
            Err(_) => return "offline".into(),
        };
        if primary.as_str() == "/" {
            return "offline".into();
        }
        let Some(a) = self.proxy(primary.as_str(), ACTIVE) else { return "offline".into() };
        let id: String = a.get_property("Id").unwrap_or_default();
        let ip = a
            .get_property::<OwnedObjectPath>("Ip4Config")
            .ok()
            .filter(|p| p.as_str() != "/")
            .and_then(|p| self.proxy(p.as_str(), IP4).and_then(|c| {
                c.get_property::<Vec<HashMap<String, OwnedValue>>>("AddressData").ok()
            }))
            .and_then(|list| list.into_iter().next())
            .and_then(|m| m.get("address").and_then(|v| String::try_from(v.clone()).ok()));
        match ip {
            Some(ip) => format!("{id} · {ip}"),
            None => id,
        }
    }

    /// Ask the card to look again. The answer arrives a few seconds later.
    pub fn scan(&self) {
        if let Some(w) = self.wifi.as_ref().and_then(|w| self.proxy(w.as_str(), WIRELESS)) {
            let none: HashMap<&str, Value> = HashMap::new();
            let _: zbus::Result<()> = w.call("RequestScan", &(none,));
        }
    }

    /// Join a network. A saved one comes up as it is; a new one is added
    /// with the password given, which NetworkManager keeps from then on,
    /// so it rejoins by itself with nothing else running.
    ///
    /// Gives back the live connection to wait on, and whether a new saved
    /// connection was made, so a failed first try can be cleaned away.
    pub fn connect(&self, net: &Net, password: Option<&str>) -> Result<(OwnedObjectPath, Option<OwnedObjectPath>), String> {
        let wifi = self.wifi.clone().ok_or("no Wi-Fi card")?;
        let nm = self.proxy(NM_PATH, NM).ok_or("no NetworkManager")?;
        if let Some(saved) = &net.saved {
            let active: OwnedObjectPath = nm
                .call("ActivateConnection", &(saved, &wifi, &net.ap))
                .map_err(|e| short(&e))?;
            return Ok((active, None));
        }
        let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
        if let Some(pw) = password {
            let mut sec: HashMap<&str, Value> = HashMap::new();
            let mgmt = if net.lock == Lock::Sae { "sae" } else { "wpa-psk" };
            sec.insert("key-mgmt", Value::from(mgmt));
            sec.insert("psk", Value::from(pw));
            settings.insert("802-11-wireless-security", sec);
        }
        let (made, active): (OwnedObjectPath, OwnedObjectPath) = nm
            .call("AddAndActivateConnection", &(settings, &wifi, &net.ap))
            .map_err(|e| short(&e))?;
        Ok((active, Some(made)))
    }

    /// Wait for a connection to come up or fail, looking a few times a
    /// second. Only while you watch a connect; never on its own.
    pub fn wait(&self, active: &OwnedObjectPath, limit: Duration) -> Outcome {
        let began = Instant::now();
        while began.elapsed() < limit {
            let state: u32 = match self.proxy(active.as_str(), ACTIVE) {
                // Gone means NetworkManager gave up on it.
                None => return Outcome::Failed,
                Some(p) => match p.get_property("State") {
                    Ok(s) => s,
                    Err(_) => return Outcome::Failed,
                },
            };
            match state {
                2 => return Outcome::Up,
                3 | 4 => return Outcome::Failed,
                _ => std::thread::sleep(Duration::from_millis(300)),
            }
        }
        Outcome::Slow
    }

    pub fn disconnect(&self, active: &OwnedObjectPath) -> Result<(), String> {
        let nm = self.proxy(NM_PATH, NM).ok_or("no NetworkManager")?;
        nm.call::<_, _, ()>("DeactivateConnection", &(active,)).map_err(|e| short(&e))
    }

    /// Forget a saved connection, password and all.
    pub fn forget(&self, saved: &OwnedObjectPath) -> Result<(), String> {
        let p = self.proxy(saved.as_str(), CONNECTION).ok_or("no such connection")?;
        p.call::<_, _, ()>("Delete", &()).map_err(|e| short(&e))
    }

    pub fn set_wifi(&self, on: bool) -> Result<(), String> {
        let nm = self.proxy(NM_PATH, NM).ok_or("no NetworkManager")?;
        nm.set_property("WirelessEnabled", on).map_err(|e| e.to_string())
    }

    pub fn vpn_up(&self, conn: &OwnedObjectPath) -> Result<OwnedObjectPath, String> {
        let nm = self.proxy(NM_PATH, NM).ok_or("no NetworkManager")?;
        let none = root();
        nm.call("ActivateConnection", &(conn, &none, &none)).map_err(|e| short(&e))
    }
}

/// A setting's text, or nothing.
fn text(s: &HashMap<String, HashMap<String, OwnedValue>>, group: &str, key: &str) -> String {
    s.get(group)
        .and_then(|g| g.get(key))
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default()
}

/// How a network is locked, from the three sets of flags an access point
/// carries. Offering WPA2 and WPA3 at once counts as WPA2, which every
/// card can join.
pub fn lock_of(flags: u32, wpa: u32, rsn: u32) -> Lock {
    const PRIVACY: u32 = 0x1;
    const PSK: u32 = 0x100;
    const EAP: u32 = 0x200;
    const SAE: u32 = 0x400;
    let both = wpa | rsn;
    if both & EAP != 0 {
        Lock::Enterprise
    } else if both & PSK != 0 {
        Lock::Psk
    } else if both & SAE != 0 {
        Lock::Sae
    } else if flags & PRIVACY != 0 || both != 0 {
        Lock::Psk
    } else {
        Lock::Open
    }
}

/// A D-Bus error without the D-Bus: just what went wrong.
fn short(e: &zbus::Error) -> String {
    let s = e.to_string();
    s.rsplit(": ").next().unwrap_or(&s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_read_from_the_flags() {
        assert_eq!(lock_of(0, 0, 0), Lock::Open);
        assert_eq!(lock_of(1, 0, 0x188), Lock::Psk, "WPA2 with a password");
        assert_eq!(lock_of(1, 0, 0x400), Lock::Sae, "WPA3 alone");
        assert_eq!(lock_of(1, 0, 0x500), Lock::Psk, "both on offer joins as WPA2");
        assert_eq!(lock_of(1, 0, 0x200), Lock::Enterprise);
        assert_eq!(lock_of(1, 0, 0), Lock::Psk, "an old privacy bit alone still wants a password");
    }

    #[test]
    fn a_dbus_error_comes_back_short() {
        let e = zbus::Error::Failure("org.freedesktop.NetworkManager.Failed: Secrets were required".into());
        assert_eq!(short(&e), "Secrets were required");
    }
}
