// The wall display: a read-only page on the home network for a spare screen
// (an old iPad on a stand). The page only ever holds percentages, reset
// times, token counts and API-rate value; never an account, email or token.
//
// Off until it's switched on from the menu. When on, it listens on port 8765
// and answers only at a secret path (http://<mac>:8765/<key>), so a guess at
// the address gets a 404. The page is plain HTML and CSS that refreshes itself
// every minute: no JavaScript, so a decade-old Safari shows it fine.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::poller::{Shared, Snapshot};

pub const PORT: u16 = 8765;
/// seconds between the page's own reloads
const REFRESH_S: u32 = 60;
static CSS: &str = include_str!("../assets/wall.css");

/// The switch and the secret path, kept beside the app's other settings.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct State {
    enabled: bool,
    key: String,
}

fn state_path() -> std::path::PathBuf {
    crate::auth::default_data_dir().join("wall-display.json")
}

fn load() -> State {
    std::fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(s: &State) {
    let path = state_path();
    let _ = std::fs::create_dir_all(crate::auth::default_data_dir());
    if let Ok(body) = serde_json::to_vec_pretty(s) {
        let _ = std::fs::write(&path, body);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// Eight characters a person can type on a tablet: no 0/o, 1/l look-alikes.
fn new_key() -> String {
    const ABC: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut buf = [0u8; 8];
    if getrandom::fill(&mut buf).is_err() {
        return String::new();
    }
    buf.iter().map(|b| ABC[(*b as usize) % ABC.len()] as char).collect()
}

/// The running page server. Dropping it stops the server.
pub struct Server {
    stop: Arc<AtomicBool>,
    key: String,
    host: Option<String>,
    ip: Option<String>,
}

impl Server {
    /// Start it if it was left on last time.
    pub fn resume(data: &Shared) -> Option<Result<Server, String>> {
        load().enabled.then(|| Server::start(data.clone()))
    }

    /// Switch on (remembered across restarts) and start listening.
    pub fn turn_on(data: &Shared) -> Result<Server, String> {
        let mut s = load();
        s.enabled = true;
        if s.key.len() < 8 {
            s.key = new_key();
        }
        save(&s);
        Server::start(data.clone())
    }

    /// Switch off for good (the key is kept, so a saved bookmark still works
    /// once it's back on).
    pub fn turn_off() {
        let mut s = load();
        s.enabled = false;
        save(&s);
    }

    fn start(data: Shared) -> Result<Server, String> {
        let key = load().key;
        if key.len() < 8 {
            return Err("Wall display: no key".into());
        }
        let listener = TcpListener::bind(("0.0.0.0", PORT)).map_err(|e| format!("Wall display: port {PORT} unavailable ({e})"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, path_key) = (stop.clone(), key.clone());
        std::thread::Builder::new()
            .name("wall-display".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => answer(stream, &path_key, &data),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(250)),
                        Err(_) => std::thread::sleep(Duration::from_secs(1)),
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Server { stop, key, host: local_host_name(), ip: lan_ip() })
    }

    /// The address to open on the other screen, by IP (always typeable).
    pub fn link(&self) -> String {
        let at = self.ip.clone().unwrap_or_else(|| "this-mac-ip".into());
        format!("http://{at}:{PORT}/{}", self.key)
    }

    /// The same address by the Mac's network name, when it has one.
    pub fn named_link(&self) -> Option<String> {
        self.host.as_ref().map(|h| format!("http://{h}.local:{PORT}/{}", self.key))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// The one server, reachable from the menu's handler as well as from frames.
static WALL: std::sync::Mutex<Option<Result<Server, String>>> = std::sync::Mutex::new(None);
static DATA: std::sync::OnceLock<Shared> = std::sync::OnceLock::new();

/// At launch: remember where readings live, and start if it was left on.
pub fn init(data: &Shared) {
    let _ = DATA.set(data.clone());
    if let Ok(mut w) = WALL.lock() {
        *w = Server::resume(data);
    }
}

/// The menu's switch: on if it's off (or failed), off if it's on.
pub fn toggle() {
    let (Some(data), Ok(mut w)) = (DATA.get(), WALL.lock()) else { return };
    if matches!(*w, Some(Ok(_))) {
        *w = None; // dropping the server stops it
        Server::turn_off();
    } else {
        *w = Some(Server::turn_on(data));
    }
}

/// The address and the switch, as the menu shows them.
pub fn menu() -> crate::platform::tray::WallMenu {
    let Ok(w) = WALL.lock() else { return Default::default() };
    match &*w {
        Some(Ok(s)) => {
            let mut lines = vec![s.link()];
            lines.extend(s.named_link());
            crate::platform::tray::WallMenu { on: true, lines, link: Some(s.link()) }
        }
        Some(Err(e)) => crate::platform::tray::WallMenu { on: false, lines: vec![e.clone()], link: None },
        None => crate::platform::tray::WallMenu::default(),
    }
}

/// Put the address on the clipboard, e.g. to AirDrop or message it to the iPad.
pub fn copy_link() {
    let Some(link) = WALL.lock().ok().and_then(|w| w.as_ref()?.as_ref().ok().map(Server::link)) else { return };
    #[cfg(target_os = "macos")]
    if let Ok(mut child) = std::process::Command::new("/usr/bin/pbcopy").stdin(std::process::Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(link.as_bytes());
        }
        let _ = child.wait();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = link;
}

/// The Mac's Bonjour name ("Aidens-Mac-mini"), for a .local address.
fn local_host_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/sbin/scutil").args(["--get", "LocalHostName"]).output().ok()?;
        let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (!name.is_empty()).then_some(name)
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// This machine's address on the local network. Connecting a UDP socket
/// sends nothing; it only asks the OS which interface it would use.
fn lan_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:80").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

/// What a request asks for.
#[derive(Debug, PartialEq)]
enum Route {
    Page,
    /// a pet's animation strip: "claude-working", "codex-fire", ...
    Pet(String),
}

/// Read one request line and answer it: the page or a pet at the secret
/// path, 404 for anything else.
fn answer(mut stream: TcpStream, key: &str, data: &Shared) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 2048];
    let mut len = 0;
    while len < buf.len() {
        match stream.read(&mut buf[len..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => len += n,
        }
        if buf[..len].windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    let line = String::from_utf8_lossy(&buf[..len]);
    let (status, kind, cache, body): (&str, &str, &str, Vec<u8>) = match route(&line, key) {
        Some(Route::Page) => {
            let page = match data.lock() {
                Ok(snap) => {
                    let prefs = crate::prefs::Prefs::load();
                    render(&snap, prefs.heads_up as f64, prefs.fire as f64, &crate::format::clock(0), key)
                }
                Err(_) => String::from("<p>Busy, try again.</p>"),
            };
            ("200 OK", "text/html; charset=utf-8", "no-store", page.into_bytes())
        }
        Some(Route::Pet(name)) => match pets::strip(&name) {
            // a day's caching: the page reloads every minute, the pet needn't
            Some(png) => ("200 OK", "image/png", "max-age=86400", png),
            None => ("404 Not Found", "text/plain", "no-store", b"Not found".to_vec()),
        },
        None => ("404 Not Found", "text/plain", "no-store", b"Not found".to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

/// "GET /<key>" is the page, "GET /<key>/pet/<name>.png" a pet; with or
/// without a trailing slash or a query.
fn route(request_line: &str, key: &str) -> Option<Route> {
    let mut parts = request_line.split_whitespace();
    if parts.next() != Some("GET") {
        return None;
    }
    let target = parts.next()?;
    let path = target.split('?').next().unwrap_or("").trim_start_matches('/').trim_end_matches('/');
    let (first, rest) = path.split_once('/').unwrap_or((path, ""));
    // compare in constant time so the key can't be found a letter at a time
    let ok = !key.is_empty() && first.len() == key.len() && first.bytes().zip(key.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    if !ok {
        return None;
    }
    if rest.is_empty() {
        return Some(Route::Page);
    }
    let name = rest.strip_prefix("pet/")?.strip_suffix(".png")?;
    name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-').then(|| Route::Pet(name.to_string()))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

/// One limit window.
struct Limit {
    label: String,
    pct: Option<f64>,
    reset_ms: Option<i64>,
    /// the window's full length, when it's known: it places the pace tick
    window_ms: Option<i64>,
}

impl Limit {
    fn new(label: &str, pct: Option<f64>, reset_ms: Option<i64>, window_ms: Option<i64>) -> Limit {
        Limit { label: label.into(), pct, reset_ms, window_ms }
    }

    fn out(&self) -> bool {
        self.pct.is_some_and(|p| p >= 100.0)
    }

    /// How far through its window this limit is, 0-100 (where an even
    /// spend would have the bar by now).
    fn elapsed(&self) -> Option<f64> {
        let (w, r) = (self.window_ms?, self.reset_ms?);
        (w > 0).then(|| ((w - r) as f64 / w as f64 * 100.0).clamp(0.0, 100.0))
    }

    /// At an even spend, when it runs out: ms from now, if that's before it
    /// resets. Said only once 3% of the window has passed and the use is
    /// clearly ahead of the clock; a straight line, so "about".
    fn runs_out_in(&self) -> Option<i64> {
        let (pct, el, w, r) = (self.pct?, self.elapsed()?, self.window_ms?, self.reset_ms?);
        if self.out() || el < 3.0 || pct < el + 5.0 || pct <= 0.0 {
            return None;
        }
        let spent_ms = w - r;
        let left_ms = ((100.0 - pct) / pct * spent_ms as f64) as i64;
        (left_ms < r).then_some(left_ms)
    }

    /// How tight it is, for picking the one that binds first: out, then
    /// the highest use (a tie goes to the later reset, the longer wait).
    fn weight(&self) -> (bool, i64, i64) {
        (self.out(), (self.pct.unwrap_or(-1.0) * 10.0) as i64, self.reset_ms.unwrap_or(0))
    }
}

/// The state of a card, or of the whole board.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
enum Level {
    Clear,
    Pace,
    Warn,
    Hot,
    Out,
}

impl Level {
    fn class(self) -> &'static str {
        match self {
            Level::Clear => "clear",
            Level::Pace => "pace",
            Level::Warn => "warn",
            Level::Hot => "hot",
            Level::Out => "out",
        }
    }
}

/// One service's card.
struct Card {
    /// css class and pet: claude, codex or cursor
    kind: &'static str,
    name: &'static str,
    plan: Option<String>,
    working: bool,
    /// shown in place of the limits (signed out, not read yet)
    note: Option<String>,
    limits: Vec<Limit>,
    /// the reading is too old to trust
    stale: Option<String>,
}

impl Card {
    /// The limit that stops you first.
    fn binding(&self) -> Option<&Limit> {
        self.limits.iter().filter(|l| l.pct.is_some()).max_by_key(|l| l.weight())
    }

    fn level(&self, warn: f64, fire: f64) -> Level {
        let Some(b) = self.binding() else { return Level::Clear };
        let p = b.pct.unwrap_or(0.0);
        if b.out() {
            Level::Out
        } else if p >= fire {
            Level::Hot
        } else if p >= warn {
            Level::Warn
        } else if self.limits.iter().any(|l| l.runs_out_in().is_some()) {
            Level::Pace
        } else {
            Level::Clear
        }
    }

    /// Which pet animation fits: asleep when the reading is stale, tired
    /// when out, on fire past the fire line, typing while it works.
    fn mood(&self, warn: f64, fire: f64) -> &'static str {
        match self.level(warn, fire) {
            _ if self.stale.is_some() || self.note.is_some() => "sleeping",
            Level::Out => "tired",
            Level::Hot => "fire",
            _ if self.working => "working",
            _ => "idle",
        }
    }
}

fn cards(snap: &Snapshot) -> Vec<Card> {
    let mut out = vec![];
    let live = |w: &crate::auth::Window| crate::components::meter::live(w, snap.limits_at);
    let old = snap.limits_at.map(|t| t.elapsed().as_millis() as i64).filter(|&ms| ms > 15 * 60_000);
    let mut claude = Card {
        kind: "claude",
        name: "Claude",
        plan: None,
        working: snap.usage.as_ref().is_some_and(|u| u.active),
        note: None,
        limits: vec![],
        stale: old.map(|ms| format!("Last reading {} ago", crate::format::countdown(ms))),
    };
    if !snap.loaded {
        claude.note = Some("Waiting for the first reading…".into());
    } else if !snap.connected {
        claude.note = Some("Signed out in TokenRunway".into());
    }
    if let Some(l) = &snap.limits {
        let s = live(&l.session);
        claude.limits.push(Limit::new("5h", s.map(|s| s.0), s.and_then(|s| s.1), Some(5 * HOUR)));
        let w = live(&l.week);
        claude.limits.push(Limit::new("Week", w.map(|w| w.0), w.and_then(|w| w.1), Some(7 * DAY)));
        for sc in &l.scoped {
            let w = live(&crate::auth::Window { pct: sc.pct, reset_ms: sc.reset_ms });
            claude.limits.push(Limit::new(&sc.label, w.map(|w| w.0), w.and_then(|w| w.1), Some(7 * DAY)));
        }
    }
    out.push(claude);

    let win = |w: &Option<crate::codex::Window>| (w.as_ref().and_then(|w| w.pct), w.as_ref().and_then(|w| w.reset_ms));
    if let Some(c) = &snap.codex {
        // a window resetting further out than a week is a plan's own: its
        // length isn't known, so it gets no pace tick
        let long = |ms: Option<i64>| if ms.is_some_and(|ms| ms > 8 * DAY) { ("Limit", None) } else { ("Week", Some(7 * DAY)) };
        let mut limits = vec![];
        let (p, r) = win(&c.session);
        if c.weekly_only {
            let (label, w) = long(r);
            limits.push(Limit::new(label, p, r, w));
        } else {
            limits.push(Limit::new("5h", p, r, Some(5 * HOUR)));
            let (wp, wr) = win(&c.weekly);
            let (label, w) = long(wr);
            limits.push(Limit::new(label, wp, wr, w));
        }
        out.push(Card { kind: "codex", name: "Codex", plan: c.plan.clone(), working: c.active, note: None, limits, stale: None });
    }
    if let Some(c) = &snap.cursor {
        let (p, r) = win(&c.session);
        let (ap, ar) = win(&c.api);
        out.push(Card {
            kind: "cursor",
            name: "Cursor",
            plan: c.plan.clone(),
            working: c.active,
            note: c.signed_out.then(|| "Signed out of Cursor".into()),
            limits: vec![Limit::new("Month", p, r, None), Limit::new("API", ap, ar, None)],
            stale: None,
        });
    }
    out
}

/// "5h", "Week" read as words in a sentence.
fn spoken(label: &str) -> String {
    match label {
        "5h" => "5-hour session".into(),
        "Week" => "week".into(),
        other => other.to_string(),
    }
}

/// The one line across the top: can I keep coding, and if not, when?
fn verdict(cards: &[Card], warn: f64, fire: f64) -> (Level, String) {
    let ready: Vec<&Card> = cards.iter().filter(|c| c.note.is_none() && c.binding().is_some()).collect();
    let Some(worst) = ready.iter().max_by(|a, b| a.level(warn, fire).partial_cmp(&b.level(warn, fire)).unwrap_or(std::cmp::Ordering::Equal)) else {
        return (Level::Clear, "Waiting for a reading".into());
    };
    let level = worst.level(warn, fire);
    let b = worst.binding().expect("ready cards have one");
    let pct = b.pct.unwrap_or(0.0);
    let text = match level {
        Level::Out => {
            let back = b.reset_ms.map(|ms| format!(" · back in {} ({})", crate::format::countdown(ms), crate::format::clock(ms))).unwrap_or_default();
            // point at the service with the most room, if it has some
            let other = ready
                .iter()
                .filter(|c| c.kind != worst.kind && c.level(warn, fire) < Level::Warn)
                .filter_map(|c| Some((c.name, 100.0 - c.binding()?.pct?)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let other = other.map(|(n, left)| format!(" · {n} has {left:.0}% left")).unwrap_or_default();
            format!("<b>{} is out</b>{back}{other}", worst.name)
        }
        Level::Hot | Level::Warn => {
            let reset = b.reset_ms.map(|ms| format!(" · resets {}", crate::format::clock(ms))).unwrap_or_default();
            format!("<b>Slow down</b> · {} {} at {pct:.0}%{reset}", worst.name, esc(&spoken(&b.label)))
        }
        Level::Pace => {
            let l = worst.limits.iter().find(|l| l.runs_out_in().is_some()).unwrap_or(b);
            let when = l.runs_out_in().map(|ms| format!(" · out in ~{} at this pace", crate::format::countdown(ms))).unwrap_or_default();
            format!("<b>Watch your pace</b> · {} {}{when}", worst.name, esc(&spoken(&l.label)))
        }
        Level::Clear => {
            let all: Vec<String> = ready.iter().filter_map(|c| Some(format!("{} {:.0}%", c.name, c.binding()?.pct?))).collect();
            format!("<b>All clear</b> · {}", all.join(" · "))
        }
    };
    (level, text)
}

fn tone(pct: Option<f64>, warn: f64, fire: f64) -> &'static str {
    match pct {
        Some(p) if p >= 100.0 => " out",
        _ => match crate::platform::tray::Tone::of(pct, warn, fire) {
            crate::platform::tray::Tone::Calm => "",
            crate::platform::tray::Tone::Warn => " warn",
            crate::platform::tray::Tone::Hot => " hot",
        },
    }
}

fn width(pct: f64) -> f64 {
    if pct.is_finite() { pct.clamp(0.0, 100.0) } else { 0.0 }
}

/// A bar with the pace tick: where an even spend would be by now.
fn bar(l: &Limit, t: &str, big: bool) -> String {
    let tick = l.elapsed().filter(|_| !l.out()).map(|e| format!("<b class=\"tick\" style=\"left:{e:.1}%\"></b>")).unwrap_or_default();
    let class = if big { "track big-track" } else { "track" };
    format!("<div class=\"{class}\"><div class=\"fill{t}\" style=\"width:{:.1}%\"></div>{tick}</div>", width(l.pct.unwrap_or(0.0)))
}

/// The line under the hero bar: the pace, then the reset.
fn pace_note(l: &Limit) -> String {
    let reset = match (l.reset_ms, l.label.as_str()) {
        (Some(ms), "5h") => format!("resets in {} · {}", crate::format::countdown(ms), crate::format::clock(ms)),
        (Some(ms), _) => format!("resets {}", crate::format::clock(ms)),
        (None, _) => String::new(),
    };
    let pace = match (l.runs_out_in(), l.elapsed(), l.pct) {
        (Some(ms), _, _) => format!("<span class=\"ahead\">ahead of pace · out in ~{}</span>", crate::format::countdown(ms)),
        (None, Some(e), Some(p)) if p <= e + 5.0 => "<span class=\"ok\">on pace</span>".into(),
        _ => String::new(),
    };
    [pace, reset].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("<br>")
}

fn card_html(c: &Card, warn: f64, fire: f64, key: &str) -> String {
    let level = c.level(warn, fire);
    let mut h = format!("<div class=\"card {} {}\">", c.kind, level.class());
    let plan = c.plan.as_deref().map(|p| format!("<span class=\"plan\">{}</span>", esc(p))).unwrap_or_default();
    let pet = format!("<span class=\"pet mood-{0}\" style=\"background-image:url(/{key}/pet/{1}-{0}.png)\"></span>", c.mood(warn, fire), c.kind);
    h += &format!("<div class=\"head\"><span class=\"name\">{}</span>{plan}</div>", c.name);
    if let Some(note) = &c.note {
        return h + &format!("<div class=\"note\">{}</div><div class=\"stage\">{pet}</div></div>", esc(note));
    }
    let Some(hero) = c.binding() else { return h + "<div class=\"note\">No reading yet</div></div>" };
    let t = tone(hero.pct, warn, fire);
    let first = if c.limits.iter().filter(|l| l.pct.is_some()).count() > 1 { " · limits you first" } else { "" };
    h += &format!("<div class=\"label\">{}{first}</div>", esc(&spoken(&hero.label)));
    if hero.out() {
        // waiting mode: the wait is the only number that matters
        let back = hero.reset_ms.map(crate::format::countdown).unwrap_or_else(|| "—".into());
        let at = hero.reset_ms.map(|ms| format!("at {}", crate::format::clock(ms))).unwrap_or_default();
        h += &format!("<div class=\"big out\"><small>back in</small> {back}</div><div class=\"when\">{at}</div>");
    } else {
        let pct = hero.pct.map(|p| format!("{p:.0}<small>%</small>")).unwrap_or_else(|| "—".into());
        h += &format!("<div class=\"big{t}\">{pct}</div>{}<div class=\"when\">{}</div>", bar(hero, t, true), pace_note(hero));
    }
    // the pet takes the room between the big number and the smaller limits
    h += &format!("<div class=\"stage\">{pet}</div><div class=\"rows\">");
    for l in c.limits.iter().filter(|l| !std::ptr::eq(*l, hero)) {
        let t = tone(l.pct, warn, fire);
        let pct = l.pct.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "—".into());
        let reset = match (l.reset_ms, l.label.as_str()) {
            (Some(ms), "5h") => crate::format::countdown(ms),
            (Some(ms), _) => crate::format::clock(ms),
            _ => String::new(),
        };
        h += &format!(
            "<div class=\"row\"><div class=\"rlabel\">{}</div>{}<div class=\"rpct{t}\">{pct}</div><div class=\"rreset\">{}</div></div>",
            esc(&l.label),
            bar(l, t, false),
            esc(&reset)
        );
    }
    if let Some(s) = &c.stale {
        h += &format!("<div class=\"stale\">{}</div>", esc(s));
    }
    h + "</div></div>"
}

/// API-rate value of the local logs, along the foot of the page: what the
/// same tokens would have cost on the API. An estimate, never a bill.
fn spend_line(snap: &Snapshot) -> String {
    if snap.usage.is_none() && snap.codex.is_none() {
        return String::new();
    }
    let sum = |f: &dyn Fn(&crate::usage::Usage) -> f64, g: &dyn Fn(&crate::codex::CodexUsage) -> f64| {
        snap.usage.as_ref().map(f).unwrap_or(0.0) + snap.codex.as_ref().map(g).unwrap_or(0.0)
    };
    let tile = |v: f64, label: &str| format!("<span class=\"tile\"><b>{}</b> {label}</span>", crate::format::usd(v));
    format!(
        "<div class=\"spend\"><span class=\"slabel\">API-rate value<small>not billed</small></span>{}{}{}</div>",
        tile(sum(&|u| u.cost_today, &|c| c.cost_today), "today"),
        tile(sum(&|u| u.cost_7d, &|c| c.cost_7d), "7 days"),
        tile(sum(&|u| u.cost_30d, &|c| c.cost_30d), "30 days")
    )
}

/// The whole page. `now` is the clock time it was drawn at; `key` builds
/// the pets' addresses.
pub fn render(snap: &Snapshot, warn: f64, fire: f64, now: &str, key: &str) -> String {
    let cards = cards(snap);
    let (level, line) = verdict(&cards, warn, fire);
    let body: String = cards.iter().map(|c| card_html(c, warn, fire, key)).collect();
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"{REFRESH_S}\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"apple-mobile-web-app-capable\" content=\"yes\"><meta name=\"mobile-web-app-capable\" content=\"yes\">\
<meta name=\"apple-mobile-web-app-status-bar-style\" content=\"black\"><meta name=\"theme-color\" content=\"#120d0a\">\
<meta name=\"robots\" content=\"noindex\"><title>TokenRunway</title><style>{CSS}</style></head><body class=\"{}\"><div class=\"page\">\
<div class=\"verdict\">{line}</div><div class=\"grid\">{body}</div>\
{}<div class=\"footer\">TokenRunway · updated {}</div></div></body></html>",
        level.class(),
        spend_line(snap),
        esc(now)
    )
}

/// The pets as the page shows them: each mood's frames cut from the app's
/// sprite sheet, shrunk to 128 px and laid in one strip that CSS steps
/// through. Made once per strip and kept, so an old tablet gets a small
/// file and the Mac does the work once.
mod pets {
    use std::collections::HashMap;
    use std::sync::Mutex;

    const OUT: usize = 128;

    static CACHE: Mutex<Option<HashMap<String, Vec<u8>>>> = Mutex::new(None);

    /// The frames of each mood, a frame per CSS step (held frames repeat).
    fn frames(mood: &str) -> Option<(bool, &'static [usize])> {
        // (from the working sheet?, frames)
        Some(match mood {
            "working" => (true, &[0, 1, 0, 1, 3, 0, 1, 2]),
            "idle" => (false, &[0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 2, 3, 3]),
            "sleeping" => (false, &[4, 5, 6, 7]),
            "fire" => (false, &[8, 9, 10, 11]),
            "tired" => (false, &[12, 13, 14, 15]),
            _ => return None,
        })
    }

    /// "claude-working" → its PNG strip.
    pub fn strip(name: &str) -> Option<Vec<u8>> {
        let mut cache = CACHE.lock().ok()?;
        let map = cache.get_or_insert_with(HashMap::new);
        if let Some(png) = map.get(name) {
            return Some(png.clone());
        }
        let (who, mood) = name.split_once('-')?;
        let (working, picks) = frames(mood)?;
        let now = chrono::Local::now();
        use chrono::Datelike;
        let prefs = crate::prefs::Prefs::load();
        let skin = match who {
            "claude" => crate::components::skins::active_theme(&prefs.pet, &prefs.theme, now.year(), now.month(), now.day()),
            "codex" => crate::components::skins::codex_for_theme(&prefs.theme, now.month(), now.day()),
            _ => return None,
        };
        let (bytes, rows) = if working { (skin.working, 2) } else { (skin.moods, 4) };
        let png = cut(bytes, 4, rows, picks)?;
        map.insert(name.to_string(), png.clone());
        Some(png)
    }

    /// Decode the sheet, box-shrink the picked frames and encode a strip.
    fn cut(sheet: &[u8], cols: usize, rows: usize, picks: &[usize]) -> Option<Vec<u8>> {
        let mut dec = png::Decoder::new(std::io::Cursor::new(sheet));
        dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
        let mut r = dec.read_info().ok()?;
        let mut buf = vec![0; r.output_buffer_size()?];
        let info = r.next_frame(&mut buf).ok()?;
        let (w, h) = (info.width as usize, info.height as usize);
        let (fw, fh) = (w / cols, h / rows);
        if fw < OUT || fh < OUT {
            return None;
        }
        let (sx, sy) = (fw / OUT, fh / OUT);
        let mut out = vec![0u8; OUT * picks.len() * OUT * 4];
        let stride = OUT * picks.len() * 4;
        for (i, &f) in picks.iter().enumerate() {
            let (c, rw) = (f % cols, f / cols);
            for y in 0..OUT {
                for x in 0..OUT {
                    // average the source block, weighting colour by alpha
                    let (mut acc, mut a_sum) = ([0u32; 3], 0u32);
                    for dy in 0..sy {
                        for dx in 0..sx {
                            let px = ((rw * fh + y * sy + dy) * w + c * fw + x * sx + dx) * 4;
                            let a = buf[px + 3] as u32;
                            for k in 0..3 {
                                acc[k] += buf[px + k] as u32 * a;
                            }
                            a_sum += a;
                        }
                    }
                    let o = y * stride + (i * OUT + x) * 4;
                    for k in 0..3 {
                        out[o + k] = acc[k].checked_div(a_sum).unwrap_or(0) as u8;
                    }
                    out[o + 3] = (a_sum / (sx * sy) as u32) as u8;
                }
            }
        }
        let mut png_out = vec![];
        {
            let mut enc = png::Encoder::new(&mut png_out, (OUT * picks.len()) as u32, OUT as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::High);
            let mut wr = enc.write_header().ok()?;
            wr.write_image_data(&out).ok()?;
        }
        Some(png_out)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn every_mood_cuts_a_strip_of_its_frames() {
            for mood in ["working", "idle", "sleeping", "fire", "tired"] {
                for who in ["claude", "codex"] {
                    let png = super::strip(&format!("{who}-{mood}")).unwrap_or_else(|| panic!("{who}-{mood}"));
                    let d = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
                    let (_, frames) = super::frames(mood).unwrap();
                    assert_eq!((d.info().width as usize, d.info().height), (128 * frames.len(), 128));
                }
            }
            assert!(super::strip("claude-dancing").is_none());
            assert!(super::strip("bob-idle").is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_secret_path_answers() {
        assert_eq!(route("GET /abcd2345 HTTP/1.1", "abcd2345"), Some(Route::Page));
        assert_eq!(route("GET /abcd2345/ HTTP/1.1", "abcd2345"), Some(Route::Page));
        assert_eq!(route("GET /abcd2345?x=1 HTTP/1.1", "abcd2345"), Some(Route::Page));
        assert_eq!(route("GET /abcd2345/pet/claude-idle.png HTTP/1.1", "abcd2345"), Some(Route::Pet("claude-idle".into())));
        assert_eq!(route("GET /abcd2345/pet/../x.png HTTP/1.1", "abcd2345"), None);
        assert_eq!(route("GET /abcd2345/other HTTP/1.1", "abcd2345"), None);
        assert_eq!(route("GET /pet/claude-idle.png HTTP/1.1", "abcd2345"), None);
        assert_eq!(route("GET / HTTP/1.1", "abcd2345"), None);
        assert_eq!(route("GET /abcd2346 HTTP/1.1", "abcd2345"), None);
        assert_eq!(route("POST /abcd2345 HTTP/1.1", "abcd2345"), None);
        assert_eq!(route("GET / HTTP/1.1", ""), None);
        assert_eq!(route("", "abcd2345"), None);
    }

    #[test]
    fn keys_are_typeable() {
        let k = new_key();
        assert_eq!(k.len(), 8);
        assert!(k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert!(!k.contains(['0', 'o', '1', 'l']));
    }

    fn limits(session: f64, s_reset: i64, week: f64, w_reset: i64) -> Snapshot {
        let mut snap = Snapshot { loaded: true, connected: true, ..Default::default() };
        snap.limits = Some(crate::auth::Limits {
            session: crate::auth::Window { pct: session, reset_ms: Some(s_reset) },
            week: crate::auth::Window { pct: week, reset_ms: Some(w_reset) },
            scoped: vec![crate::auth::Scoped { label: "<b>Fable</b>".into(), pct: 10.0, reset_ms: None }],
        });
        snap
    }

    #[test]
    fn the_tightest_limit_leads_and_labels_are_escaped() {
        let page = render(&limits(46.0, 4 * HOUR, 84.0, 3 * DAY), 80.0, 90.0, "11:00 PM", "k");
        assert!(page.contains("week · limits you first"));
        assert!(page.contains("<b>Slow down</b> · Claude week at 84%"));
        assert!(page.contains("&lt;b&gt;Fable"));
        assert!(!page.contains("<b>Fable"));
        assert!(!page.contains("<script"));
    }

    #[test]
    fn out_shows_the_wait() {
        let page = render(&limits(100.0, 2 * HOUR, 40.0, 3 * DAY), 80.0, 90.0, "now", "k");
        assert!(page.contains("<b>Claude is out</b> · back in 2h 0m"));
        assert!(page.contains("<small>back in</small> 2h 0m"));
        assert!(page.contains("claude-tired.png"));
        assert!(page.contains("body class=\"out\""));
    }

    #[test]
    fn pace_is_judged_against_the_clock() {
        // 2h into a 5h window (40% of it) with 70% used: ahead, out in ~51m
        let l = Limit::new("5h", Some(70.0), Some(3 * HOUR), Some(5 * HOUR));
        assert_eq!(l.elapsed(), Some(40.0));
        assert_eq!(l.runs_out_in().map(|ms| ms / 60_000), Some(51));
        // 30% used at 40% of the window: on pace, no projection
        assert_eq!(Limit::new("5h", Some(30.0), Some(3 * HOUR), Some(5 * HOUR)).runs_out_in(), None);
        // too early in the window to say
        assert_eq!(Limit::new("5h", Some(10.0), Some(5 * HOUR - 60_000), Some(5 * HOUR)).runs_out_in(), None);
        // a window of unknown length gets no tick
        assert_eq!(Limit::new("Limit", Some(10.0), Some(20 * DAY), None).elapsed(), None);
        let page = render(&limits(70.0, 3 * HOUR, 20.0, 6 * DAY), 80.0, 90.0, "now", "k");
        assert!(page.contains("<b>Watch your pace</b> · Claude 5-hour session · out in ~51m"));
    }

    #[test]
    fn a_signed_out_page_says_so() {
        let snap = Snapshot { loaded: true, connected: false, ..Default::default() };
        let page = render(&snap, 80.0, 90.0, "now", "k");
        assert!(page.contains("Signed out in TokenRunway"));
        assert!(page.contains("claude-sleeping.png"));
    }
}
