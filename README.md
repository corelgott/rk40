# rk40

> **Abgeleitet von [K40 Whisperer](https://www.scorchworks.com/K40whisperer/k40whisperer.html)**
> von Scorch (scorchworks.com). Rust-Neuimplementierung des Protokoll- und
> Steuerkerns — **kein offizielles Scorch-Projekt**. Details: [NOTICE](NOTICE).

Single-User-Webserver zur Steuerung eines **K40 Laser-Cutters** (M2-Nano-Board,
LHYMICRO-GL-Protokoll) — geschrieben in Rust.

Ein einzelnes Binary ohne Laufzeit-Abhängigkeiten: Webserver, SVG-Slicer,
USB-Treiber und Weboberfläche sind eingebaut. Läuft auf **macOS, Linux und
Windows**.

## Funktionen

- **SVG laden & slicen** – SVGs werden direkt gerendert (kein Inkscape nötig)
  und in fertige EGV-Daten für das M2-Nano-Board übersetzt.
- **Raster-Gravur** – Scanline-Gravur mit wählbarem Zeilenabstand
  (0,025–0,1 mm), Auflösung, optionalem Floyd-Steinberg-Dithering und
  Invertierung.
- **Vektor-Gravur & Vektor-Schnitt** – Pfade werden mit hoher Genauigkeit
  abgeflacht und als Cut-Linien gefahren.
- **Farb-Zuordnung** – Die Farben des SVGs werden erkannt; pro Farbe wählbar:
  Gravur (Raster), Vektor-Gravur, Schnitt oder Ignorieren
  (Voreinstellung: Rot `#ff0000` = Schnitt, Blau `#0000ff` = Vektor-Gravur,
  Rest = Gravur — wie bei K40 Whisperer). Ein Job kombiniert alle Operationen
  in der Reihenfolge Gravur → Vektor-Gravur → Schnitt, mit eigenem Tempo und
  eigenen Durchgängen pro Operation.
- **Skalierung** – per Zielmaß (Breite/Höhe in mm), Faktor oder direkt am
  Eck-Anfasser auf der Arbeitsfläche.
- **Hell/Dunkel** – umschaltbares Design im Header (folgt initial der
  System-Einstellung) und separat umschaltbarer Hintergrund der Arbeitsfläche
  („Fläche“) — standardmäßig hell, damit schwarze SVGs gut sichtbar sind.
  Beides wird im Browser gemerkt.
- **Direkt drucken oder als Projekt speichern** – „Drucken“ sliced und startet
  in einem Schritt; „Als Projekt speichern“ legt den fertig berechneten Job
  (`egv.bin`) in der Projektliste ab — erneutes Drucken braucht die SVG nicht
  mehr. Projekte lassen sich als `.egv` (K40-Whisperer-kompatibel) und `.svg`
  herunterladen und jederzeit wieder auf die Stage laden.
- **Arbeitskopie auf dem Server** – Stage, SVG, Position und alle Parameter
  überleben Seiten-Reload und Server-Neustart; „Stage zurücksetzen“ räumt auf.
  Die Parameter-Defaults sind über die GUI änderbar und werden serverseitig
  gespeichert.
- **Positions-Marker & Werkstück** – Kleine Ecken-Marker der Design-Bounding-Box
  lassen sich separat lasern (z. B. auf eine Unterlage), um das Werkstück exakt
  zu platzieren; der Werkstück-Umriss kann als sperrbares Rechteck auf der
  Arbeitsfläche angezeigt werden.
- **Multi-Pass ohne erneutes Nullen** – Mehrere Durchgänge laufen direkt
  hintereinander; zwischen den Passes wird *nicht* die Home-Position
  angefahren.
- **Offline-Betrieb** – SVGs können ohne angeschlossenen Laser vorbereitet,
  gespeichert und in der Vorschau geprüft werden.
- **Laser-Steuerung im Browser** – Home, Entriegeln, Not-Stopp, Pause,
  Jog-Steuerung, Live-Fortschritt (SSE), Platzierung per Drag auf der
  300 × 200-mm-Arbeitsfläche.

## Installation (Binary-Releases)

Vorgefertigte Builds erscheinen unter
[Releases](https://github.com/corelgott/rk40/releases), sobald ein Versions-Tag
gesetzt wird (z. B. `v0.1.0`). Binary-Builds laufen **nur bei Tags** `v*`,
nicht bei jedem Push.

```bash
git tag v0.1.0
git push origin v0.1.0
```

Artefakte pro Plattform: Linux- und Windows-Binary, macOS-Binary sowie eine
unsignierte `rk40.app` (siehe unten).

## Bauen

Voraussetzung: [Rust](https://rustup.rs) (stable). Die USB-Bibliothek
(libusb) wird mitkompiliert (`vendored`), es sind keine System-Pakete nötig —
nur unter Linux wird `pkg-config`/Build-Tools vorausgesetzt.

```bash
cargo build --release
./target/release/rk40 --port 8040
```

Dann im Browser: <http://localhost:8040>

| Option | Bedeutung | Default |
|---|---|---|
| `--port`, `-p` | HTTP-Port | `8040` |
| `--data-dir`, `-d` | Ablage für gespeicherte Jobs | `~/.rk40` |

### USB-Zugriff

Der K40 meldet sich als USB-Gerät `1a86:5512` (CH341).

- **macOS**: funktioniert direkt, keine Treiber nötig.
- **Linux**: der `ch341`-Kernel-Treiber wird automatisch abgekoppelt.
  Für den Zugriff ohne root eine udev-Regel anlegen:
  ```
  # /etc/udev/rules.d/99-k40.rules
  SUBSYSTEM=="usb", ATTRS{idVendor}=="1a86", ATTRS{idProduct}=="5512", MODE="0666"
  ```
- **Windows**: mit [Zadig](https://zadig.akeo.ie) den *libusb-win32*- oder
  *WinUSB*-Treiber für das Gerät installieren (wie bei K40 Whisperer).

### macOS: unsignierte App freigeben

Die Release-Datei `rk40-macos-*.app.zip` enthält eine unsignierte `.app`
(ohne Apple Developer ID). Gatekeeper blockiert den ersten Start — das ist
erwartet.

1. Zip entpacken.
2. **Rechtsklick** auf `rk40.app` → **Öffnen** → im Dialog bestätigen.
3. Falls nötig: *Systemeinstellungen* → *Datenschutz & Sicherheit* →
   „Trotzdem öffnen“.
4. Alternativ Quarantäne-Flag entfernen:
   ```bash
   xattr -cr rk40.app
   ```

Die App startet den lokalen Server und öffnet den Browser unter
<http://localhost:8040>. Zum Beenden die App im Dock beenden (oder den
Prozess im Activity Monitor).

Lokal selbst bauen:

```bash
cargo build --release
./scripts/package-macos-app.sh
```

## Arbeitsablauf

1. **SVG laden** (Drag & Drop), auf der Arbeitsfläche platzieren.
2. Modus, Tempo, Durchgänge etc. wählen und **„Vorbereiten & Speichern“** —
   der Job wird gesliced und landet in der Bibliothek.
3. Laser **verbinden**, Kopf mit **Home** nullen.
4. **Drucken** — die Position ist im Job eingerechnet; der Kopf fährt vom
   Home-Punkt los und kehrt danach zurück. Der Job kann beliebig oft
   wiederholt werden.

> **Hinweis Leistung:** Das M2-Nano-Board kann die Laserleistung nicht per
> Software regeln — sie wird wie gewohnt am Potentiometer des K40 eingestellt.

## Zuverlässigkeit während des Jobs

Der K40 hat nur einen winzigen Puffer — reißt der USB-Datenstrom ab, bleibt
der Kopf mitten im Job stehen. rk40 schützt dagegen:

- **Schlaf-Sperre**: Während ein Job läuft, verhindert rk40 den Ruhezustand
  des Rechners (macOS: `caffeinate`, Linux: `systemd-inhibit`). Unter Windows
  bitte den Energiesparmodus für die Dauer des Jobs deaktivieren.
- **USB-Reconnect**: Bei USB-Aussetzern (beim K40 häufig durch
  Einstreuungen des Laser-Netzteils) verbindet rk40 das Gerät automatisch neu
  und setzt die Übertragung fort — aufgegeben wird erst nach ~2 Minuten.
- **Sichtbare Fehler**: Bricht ein Job doch ab, erscheint ein rotes Banner
  mit dem Grund; Details landen zusätzlich im Server-Log (stderr).

Wenn Jobs wiederholt mitten drin abbrechen: kurzes, gut geschirmtes USB-Kabel
mit Ferritkern verwenden, nicht über einen Hub gehen, und den K40 gut erden —
das sind die üblichen Ursachen bei diesen Geräten.

## Architektur

```
src/
├── main.rs         Serverstart, CLI-Argumente
├── api.rs          REST-API + SSE + eingebettete SPA
├── device.rs       USB-Treiber (rusb): Pakete, CRC, Statuscodes, Job-Loop
├── egv.rs          EGV/LHYMICRO-GL-Encoder (Port von egv.py)
├── laser_speed.rs  Speed-Codes (Port von LaserSpeed.py, M2-Board)
├── planner.rs      SVG → Scanlines/Polylinien → EGV (resvg, Dithering)
└── jobs.rs         Job-Ablage (meta.json, egv.bin, preview.png, source.svg)
static/index.html   Single-Page-App (Vanilla JS, keine Build-Tools)
```

Der Protokoll-Kern ist gegen das Original verifiziert: identische Eingaben
erzeugen **byte-identische** EGV-Ströme wie `egv.py` aus K40 Whisperer
(siehe `examples/dump_egv.rs`), die Speed-Codes sind gegen `LaserSpeed.py`
getestet (`cargo test`).

## Lizenz

GPL-3.0-or-later — siehe [LICENSE](LICENSE) und [NOTICE](NOTICE).
Abgeleitet von K40 Whisperer, © Scorch (scorchworks.com).
