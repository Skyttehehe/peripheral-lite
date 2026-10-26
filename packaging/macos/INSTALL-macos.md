# peripheral-lite on macOS

The macOS build has been tested on an Apple Silicon Mac with an **MX Master 4 over
Bluetooth**: listing, info, battery, DPI read/write, presets, the background watcher, login
autostart, the app and its menu bar item. Other devices and connections (LIGHTSPEED/Bolt
receivers, headsets) use the same code as on Windows but have not been tried on a Mac yet; a
[device report](https://github.com/Skyttehehe/peripheral-lite/issues/new?template=device-report.md)
is very welcome.

No driver or kernel extension is needed. Recent macOS versions do ask for **Input
Monitoring** before a program may open a mouse or keyboard (seen on an MX Master 3S and MX Keys
S): the app asks for it itself, see step 3; for the command-line tool, allow the terminal you
run it in.

## 1. Let macOS run the downloaded files (once)

The binaries are not signed or notarised, so Gatekeeper blocks them when they come from a
download. Remove the quarantine flag from the unpacked folder:

```sh
xattr -dr com.apple.quarantine peripheral-lite-*-macos-*
```

(Or build from source, see the README; locally built files are not quarantined.)

## 2. Try the command-line tool

```sh
./peripheral-lite devices
./peripheral-lite battery b042
```

`<dev>` is what `devices` prints: a Bluetooth device appears by its product ID (`b042` for an
MX Master 4), a device behind a receiver as `receiver-PID:slot`.

If something does not work, run `./peripheral-lite report > report.txt` and attach `report.txt`
to a device report: it holds everything needed to find the problem.

## 3. The app (optional)

Move `peripheral-lite.app` to `/Applications` and open it. It shows the battery widget and a
battery item in the menu bar (Open / Skins… / Quit). Closing the widget hides it to the menu
bar.

The first time, macOS asks whether peripheral-lite may use Input Monitoring; allow it. Without
it the widget says "macOS blocks access to the devices", with a button to **System Settings >
Privacy & Security > Input Monitoring**: switch peripheral-lite on there, and the devices
appear within 30 seconds. Since the app is not signed, macOS treats every new version as a new
app: after an update, an older peripheral-lite entry there may look on but not apply. Remove
it with **−**; the app then asks again.

**Tray** in the app, or `peripheral-lite autostart on`, starts it at login through a LaunchAgent,
`~/Library/LaunchAgents/io.github.peripheral-lite.plist`. Settings, presets, skins and battery
recordings live in `~/Library/Application Support/peripheral-lite/`.

## Logi Options+

The MX series is normally managed by Logi Options+. peripheral-lite does not need it, and can run
next to it (it uses its own HID++ software ID, so replies do not get mixed up), but Options+
may set its own DPI again. For predictable results, quit Options+ while using peripheral-lite.

## Known gaps

- MX devices have no report-rate or onboard-profile features, so `rate`, `mode`,
  `profiles` and `dump` report "feature not supported" for them. That is the device, not
  macOS.
- DPI changes made by other software (or the mouse's own buttons) are not reported live for
  devices with feature 0x2201, the MX series among them: peripheral-lite only listens for 0x2202
  DPI events so far.
- Running next to Logi Options+ has not been tested.
