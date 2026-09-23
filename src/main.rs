//! roam: Wi-Fi and VPN in the terminal.
//!
//! The networks in the air, the ones you have used, what you are on now,
//! and the few things you do with them: join, leave, forget, look again,
//! and switch the radio off. It talks straight to NetworkManager, and it
//! runs only while you look at it, where nm-applet sat in memory all day.

mod nm;

use crust::{seq, style, Crust, Cursor, Input, Pane};
use nm::{Lock, Look, Nm, Outcome};
use std::io::Write;
use std::time::{Duration, Instant};

const RUST_RGB: (u8, u8, u8) = (247, 76, 0);
const HEAD_RGB: (u8, u8, u8) = (247, 140, 60);
const LIVE_RGB: (u8, u8, u8) = (120, 230, 140);
const IDLE_RGB: (u8, u8, u8) = (110, 110, 125);
const DIM_RGB: (u8, u8, u8) = (140, 140, 150);
const BAR_BG: (u8, u8, u8) = (38, 38, 38);
const PICK_BG: (u8, u8, u8) = (52, 48, 60);

/// How wide the network names are drawn.
const NAME_W: usize = 30;

/// A row you can put the cursor on.
#[derive(Clone, Copy)]
enum Row {
    Net(usize),
    Vpn(usize),
}

struct Roam {
    nm: Nm,
    look: Look,
    sel: usize,
    top: usize,
    note: String,
    /// When a scan was asked for, the moment its answer should be in.
    rescan_at: Option<Instant>,
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    if arg == "-v" || arg == "--version" {
        println!("roam {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if arg == "-h" || arg == "--help" {
        println!("roam — Wi-Fi and VPN in the terminal, straight to NetworkManager");
        println!();
        println!("  j k / arrows   move");
        println!("  Enter          join the network, or bring the VPN up or down");
        println!("  d              leave the network you are on");
        println!("  f              forget a saved network, password and all");
        println!("  r              look for networks again");
        println!("  w              Wi-Fi radio on or off");
        println!("  q              quit");
        return;
    }
    let nm = match Nm::new() {
        Ok(nm) => nm,
        Err(e) => {
            eprintln!("roam: cannot reach NetworkManager on the system bus: {e}");
            std::process::exit(1);
        }
    };
    let look = nm.look();
    let mut roam = Roam { nm, look, sel: 0, top: 0, note: String::new(), rescan_at: None };
    if !roam.nm.has_wifi() {
        roam.note = "no Wi-Fi card here; VPNs only".into();
    }
    Crust::init();
    Crust::set_app_identity("roam");
    roam.run();
    Crust::cleanup();
}

impl Roam {
    fn rows(&self) -> Vec<Row> {
        let mut r: Vec<Row> = (0..self.look.nets.len()).map(Row::Net).collect();
        r.extend((0..self.look.vpns.len()).map(Row::Vpn));
        r
    }

    fn run(&mut self) {
        loop {
            self.draw();
            // Wait for a key. Nothing wakes this but a key, or the moment
            // a scan asked for should have its answer.
            let wait = match self.rescan_at {
                Some(at) => at.saturating_duration_since(Instant::now()).as_millis().max(1) as u64,
                None => 600_000,
            };
            let Some(key) = Input::getchr_ms(wait) else {
                if self.rescan_at.is_some_and(|at| Instant::now() >= at) {
                    self.rescan_at = None;
                    self.refresh();
                    self.note = "looked again".into();
                }
                continue;
            };
            let rows = self.rows();
            match key.as_str() {
                "q" | "ESC" => return,
                "j" | "DOWN" => {
                    if self.sel + 1 < rows.len() {
                        self.sel += 1;
                    }
                }
                "k" | "UP" => self.sel = self.sel.saturating_sub(1),
                "g" | "HOME" => self.sel = 0,
                "G" | "END" => self.sel = rows.len().saturating_sub(1),
                "ENTER" => self.enter(),
                "d" => self.leave(),
                "f" => self.forget(),
                "r" => {
                    self.nm.scan();
                    self.rescan_at = Some(Instant::now() + Duration::from_secs(3));
                    self.note = "looking for networks…".into();
                }
                "w" => {
                    let on = !self.look.wifi_on;
                    self.note = match self.nm.set_wifi(on) {
                        Ok(()) => format!("Wi-Fi {}", if on { "on" } else { "off" }),
                        Err(e) => format!("Wi-Fi stayed as it was: {e}"),
                    };
                    std::thread::sleep(Duration::from_millis(400));
                    self.refresh();
                }
                "RESIZE" => {}
                _ => {}
            }
        }
    }

    fn refresh(&mut self) {
        self.look = self.nm.look();
        let n = self.rows().len();
        if self.sel >= n {
            self.sel = n.saturating_sub(1);
        }
    }

    /// Enter: join the network under the cursor, or switch a VPN.
    fn enter(&mut self) {
        let Some(row) = self.rows().get(self.sel).copied() else { return };
        match row {
            Row::Net(i) => self.join(i),
            Row::Vpn(i) => {
                let vpn = self.look.vpns[i].clone();
                self.note = match &vpn.active {
                    Some(a) => match self.nm.disconnect(a) {
                        Ok(()) => format!("{} down", vpn.name),
                        Err(e) => format!("{} stayed up: {e}", vpn.name),
                    },
                    None => {
                        self.say(&format!("bringing {} up…", vpn.name));
                        match self.nm.vpn_up(&vpn.conn) {
                            Ok(a) => match self.nm.wait(&a, Duration::from_secs(30)) {
                                Outcome::Up => format!("{} up", vpn.name),
                                Outcome::Failed => format!("{} would not come up", vpn.name),
                                Outcome::Slow => format!("{} is still coming up", vpn.name),
                            },
                            Err(e) => format!("{} would not start: {e}", vpn.name),
                        }
                    }
                };
                self.refresh();
            }
        }
    }

    fn join(&mut self, i: usize) {
        let net = self.look.nets[i].clone();
        if net.active.is_some() {
            self.note = format!("already on {}", net.ssid);
            return;
        }
        if net.lock == Lock::Enterprise && net.saved.is_none() {
            self.note = format!("{} wants a user name and a certificate; roam cannot set that up yet", net.ssid);
            return;
        }
        // A password only for a locked network nobody has saved.
        let password = if net.saved.is_none() && net.lock != Lock::Open {
            match self.ask_password(&net.ssid) {
                Some(p) if !p.is_empty() => Some(p),
                _ => {
                    self.note = String::new();
                    return;
                }
            }
        } else {
            None
        };
        self.say(&format!("joining {}…", net.ssid));
        self.note = match self.nm.connect(&net, password.as_deref()) {
            Err(e) => format!("could not join {}: {e}", net.ssid),
            Ok((active, made)) => match self.nm.wait(&active, Duration::from_secs(25)) {
                Outcome::Up => format!("on {}", net.ssid),
                Outcome::Slow => format!("{} is still coming up", net.ssid),
                Outcome::Failed => {
                    // A first try that failed leaves nothing behind, so the
                    // next try asks for the password again.
                    if let Some(m) = made {
                        let _ = self.nm.forget(&m);
                    }
                    if password.is_some() {
                        format!("{} did not take that password", net.ssid)
                    } else {
                        format!("could not join {}", net.ssid)
                    }
                }
            },
        };
        self.refresh();
    }

    /// d: leave the network under the cursor if it is the live one, or
    /// the live one wherever the cursor is.
    fn leave(&mut self) {
        let live = self.look.nets.iter().find(|n| n.active.is_some()).cloned();
        let Some(net) = live else {
            self.note = "not on any Wi-Fi".into();
            return;
        };
        let Some(active) = &net.active else { return };
        self.note = match self.nm.disconnect(active) {
            Ok(()) => format!("left {}", net.ssid),
            Err(e) => format!("still on {}: {e}", net.ssid),
        };
        std::thread::sleep(Duration::from_millis(400));
        self.refresh();
    }

    /// f: forget a saved network, after a yes.
    fn forget(&mut self) {
        let Some(Row::Net(i)) = self.rows().get(self.sel).copied() else {
            self.note = "only a saved Wi-Fi network can be forgotten here".into();
            return;
        };
        let net = self.look.nets[i].clone();
        let Some(saved) = &net.saved else {
            self.note = format!("{} is not saved", net.ssid);
            return;
        };
        self.say(&format!("forget {}, password and all? y/n", net.ssid));
        if Input::getchr_ms(60_000).as_deref() != Some("y") {
            self.note = String::new();
            return;
        }
        self.note = match self.nm.forget(saved) {
            Ok(()) => format!("forgot {}", net.ssid),
            Err(e) => format!("kept {}: {e}", net.ssid),
        };
        self.refresh();
    }

    fn ask_password(&mut self, ssid: &str) -> Option<String> {
        let (cols, rows) = Crust::terminal_size();
        let mut p = Pane::new(1, rows, cols, 1, 255, 236);
        p.scroll = false;
        p.secret = true;
        p.ask_or_cancel(&format!(" Password for {ssid}: "), "")
    }

    /// Put a line in the status bar at once, before something slow.
    fn say(&mut self, text: &str) {
        self.note = text.to_string();
        self.draw();
    }

    fn draw(&mut self) {
        let (cols, rows) = Crust::terminal_size();
        let w = cols as usize;
        let mut out = String::new();

        // The bar across the top: what you are on. Every piece carries
        // the bar's background, spaces included, so it runs unbroken.
        let now = format!("   {}", self.look.now);
        let used = 1 + "roam".len() + crust::display_width(&now);
        out.push_str(&format!(
            "{}{}{}{}",
            Cursor::at(1, 1),
            style::rgb(" ", None, Some(BAR_BG), ""),
            style::rgb("roam", Some(RUST_RGB), Some(BAR_BG), "b"),
            style::rgb(&format!("{now}{}", " ".repeat(w.saturating_sub(used))), Some((220, 220, 225)), Some(BAR_BG), "")
        ));

        // The list, scrolled so the cursor stays in view.
        let list_rows = (rows as usize).saturating_sub(4);
        let all = self.rows();
        if self.sel < self.top {
            self.top = self.sel;
        }
        if self.sel >= self.top + list_rows.saturating_sub(2) {
            self.top = self.sel + 3 - list_rows.min(self.sel + 3);
        }
        let mut lines: Vec<String> = Vec::new();
        let radio = if self.look.wifi_on {
            style::rgb("on", Some(LIVE_RGB), None, "")
        } else {
            style::rgb("off", Some(IDLE_RGB), None, "")
        };
        lines.push(format!(" {}  {}", style::rgb("Wi-Fi", Some(HEAD_RGB), None, "b"), radio));
        let mut picked_line = None;
        for (k, row) in all.iter().enumerate() {
            if k == self.look.nets.len() && !self.look.vpns.is_empty() {
                lines.push(String::new());
                lines.push(format!(" {}", style::rgb("VPN", Some(HEAD_RGB), None, "b")));
            }
            let text = match *row {
                Row::Net(i) => self.net_line(i),
                Row::Vpn(i) => self.vpn_line(i),
            };
            if k == self.sel {
                picked_line = Some(lines.len());
            }
            lines.push(text);
        }
        if self.look.nets.is_empty() {
            let why = if !self.look.wifi_on { "the radio is off; w turns it on" } else { "no networks in the air; r looks again" };
            lines.insert(1, format!("   {}", style::rgb(why, Some(DIM_RGB), None, "i")));
        }
        let start = picked_line.map(|p| p.saturating_sub(list_rows.saturating_sub(1))).unwrap_or(0).min(self.top.max(0));
        let start = if let Some(p) = picked_line { if p < start || p >= start + list_rows { p.saturating_sub(list_rows / 2) } else { start } } else { 0 };
        for r in 0..list_rows {
            let y = 3 + r as u16;
            let body = lines.get(start + r).cloned().unwrap_or_default();
            let body = if Some(start + r) == picked_line { pick(&body, w) } else { body };
            out.push_str(&format!("{}{}{}", Cursor::at(1, y), body, seq::ERASE_EOL));
        }

        // The bar along the bottom: what just happened, or the keys.
        let foot = if self.note.is_empty() {
            "Enter join · d leave · f forget · r look again · w radio · q quit".to_string()
        } else {
            self.note.clone()
        };
        // The version at the far right, as across the suite.
        let version = format!("v{} ", env!("CARGO_PKG_VERSION"));
        let room = w.saturating_sub(version.len() + 3);
        let foot = format!(" {}", take_cells(&foot, room));
        let pad = w.saturating_sub(crust::display_width(&foot) + version.len());
        out.push_str(&format!(
            "{}{}{}",
            Cursor::at(1, rows),
            style::rgb(&format!("{foot}{}", " ".repeat(pad)), Some((200, 200, 205)), Some(BAR_BG), ""),
            style::rgb(&version, Some(DIM_RGB), Some(BAR_BG), "")
        ));
        print!("{out}");
        std::io::stdout().flush().ok();
    }

    fn net_line(&self, i: usize) -> String {
        let n = &self.look.nets[i];
        let mark = if n.active.is_some() {
            style::rgb("●", Some(LIVE_RGB), None, "")
        } else {
            " ".to_string()
        };
        let name = fit(&n.ssid, NAME_W);
        let name = if n.active.is_some() {
            style::rgb(&name, Some(LIVE_RGB), None, "b")
        } else if n.saved.is_some() {
            style::rgb(&name, Some((225, 225, 230)), None, "")
        } else {
            style::rgb(&name, Some(DIM_RGB), None, "")
        };
        let lock = style::rgb(&format!("{:<6}", n.lock.label()), Some(if n.lock == Lock::Open { (230, 180, 90) } else { DIM_RGB }), None, "");
        let saved = if n.saved.is_some() { style::rgb("saved", Some(IDLE_RGB), None, "") } else { String::new() };
        format!(" {mark} {name}  {}  {lock}  {saved}", bars(n.strength))
    }

    fn vpn_line(&self, i: usize) -> String {
        let v = &self.look.vpns[i];
        let (mark, state) = if v.active.is_some() {
            (style::rgb("●", Some(LIVE_RGB), None, ""), style::rgb("up", Some(LIVE_RGB), None, ""))
        } else {
            (" ".to_string(), style::rgb("down", Some(IDLE_RGB), None, ""))
        };
        format!(" {mark} {}  {state}", style::rgb(&fit(&v.name, NAME_W), Some((225, 225, 230)), None, ""))
    }
}

/// Signal as four bars, lit to the strength.
fn bars(strength: u8) -> String {
    let glyphs = ['▂', '▄', '▆', '█'];
    let lit = match strength {
        0..=19 => 1,
        20..=44 => 2,
        45..=69 => 3,
        _ => 4,
    };
    let colour = if lit >= 3 { LIVE_RGB } else if lit == 2 { (230, 180, 90) } else { (220, 90, 80) };
    let mut s = String::new();
    for (k, g) in glyphs.iter().enumerate() {
        let c = if k < lit { colour } else { (60, 60, 68) };
        s.push_str(&style::rgb(&g.to_string(), Some(c), None, ""));
    }
    s
}

/// A row with the cursor on it: the same text on a lifted background,
/// out to the edge.
fn pick(line: &str, w: usize) -> String {
    let pad = w.saturating_sub(crust::display_width(line));
    let body = format!("{line}{}", " ".repeat(pad));
    let arm = style::rgb("", None, Some(PICK_BG), "");
    let arm = arm.trim_end_matches(style::RESET).to_string();
    format!("{arm}{}{}", body.replace(style::RESET, &format!("{}{arm}", style::RESET)), style::RESET)
}

/// Exactly `w` cells: cut with a mark when too long, padded when short.
/// Cells, not characters, since a network name may hold an emoji.
fn fit(s: &str, w: usize) -> String {
    if crust::display_width(s) <= w {
        let pad = w - crust::display_width(s);
        return format!("{s}{}", " ".repeat(pad));
    }
    let mut t = take_cells(s, w.saturating_sub(1));
    t.push('…');
    let pad = w.saturating_sub(crust::display_width(&t));
    format!("{t}{}", " ".repeat(pad))
}

/// The first `max` cells of `s`, never cutting a glyph in two.
fn take_cells(s: &str, max: usize) -> String {
    let mut walker = crust::WidthWalker::new();
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let add = walker.push(c);
        if w + add > max {
            break;
        }
        w += add;
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_always_its_column_wide() {
        assert_eq!(crust::display_width(&fit("home", 10)), 10);
        assert_eq!(crust::display_width(&fit("☕ café wifi", 10)), 10);
        assert_eq!(crust::display_width(&fit("a very long network name indeed", 10)), 10);
        assert!(fit("a very long network name indeed", 10).contains('…'));
    }

    #[test]
    fn four_bars_light_with_the_signal() {
        let lit = |s: u8| crust::strip_ansi(&bars(s)).chars().count();
        assert_eq!(lit(5), 4, "four glyphs always, some of them dark");
    }
}
