# roam

<img src="img/roam.svg" align="right" width="150">

**Wi-Fi and VPN in the terminal, and nothing running while you are not looking.**

![Rust](https://img.shields.io/badge/language-Rust-orange) ![Unlicense](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux-blue) ![Stay Amazing](https://img.shields.io/badge/Stay-Amazing-important)

The networks in the air, the ones you have used, and what you are on now. Part of the [Fe₂O₃ Rust terminal suite](https://github.com/isene/fe2o3).

nm-applet sat in memory all day to do this, holding 98 MB. roam runs when you open it and is gone when you close it.

## Using it

```bash
roam
```

| Key | Does |
|---|---|
| `j` `k` or arrows | Move |
| `Enter` | Join the network, or bring a VPN up or down |
| `d` | Leave the network you are on |
| `f` | Forget a saved network, password and all |
| `m` | Mark a saved network metered, or take the mark off |
| `r` | Look for networks again |
| `w` | Wi-Fi radio on or off |
| `?` | This list, in a box |
| `q` | Quit |

A locked network nobody has saved asks for its password, hidden as you type. A saved one joins without asking.

## Off the hotspot when you get home

`roam --watch` stays in the background. While you are on a metered network, such as your phone's hotspot, it listens for networks coming into range. When a saved one with a password shows up, it switches to it.

A password stands in for "no login page": the open networks you have saved are the airports, cafés and hotels. Of several, it takes the one NetworkManager would pick: highest priority, then the one used last.

If a VPN was up, it comes back up on the new network. A VPN that NetworkManager runs needs nothing more. A tunnel of your own, such as openfortivpn, needs its commands in `~/.roamrc`:

```
vpn_up = work-vpn
vpn_down = work-vpn stop
```

If the new network shows a login page after all, roam goes back and skips that network from then on. The skipped ones are listed in `~/.roam/portals`.

Mark the hotspot metered once with `m`, while it is in range. A notification tells you when roam switches.

On an unmetered network the watcher does nothing at all: no wake-ups, measured over 30 seconds. On a metered one it wakes only when NetworkManager's own scans find a new access point. Start it with your session, for instance from `.tilerc` or `.xinitrc`. A second one stops at once.

## How it talks

Straight to NetworkManager over the system bus, the way nm-applet does. No `nmcli` is started.

A password travels inside one call to NetworkManager, so it never shows in a process list or lands in a file.

NetworkManager keeps the password from then on, so the network rejoins by itself with nothing else running.

A first try that fails leaves nothing behind, so the next try asks for the password again.

## What it does not do yet

Networks that want a user name and a certificate (work networks, eduroam). Those still need `nmcli`.

## Install

```bash
cargo install --path .
```

## License

Public domain. Do what you like with it.
