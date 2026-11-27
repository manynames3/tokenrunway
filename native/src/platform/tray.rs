// Clauddy in the menu bar: a template icon (macOS tints it for the theme)
// with the session % beside it. A click opens its menu: the limits first
// (session, week, per-model weeks, each with a bar and its reset), the spend,
// a Details submenu for the fine print, then Open, back to floating, and Quit. On Windows and Linux it's a tray icon with the % in
// its tooltip.

use eframe::egui::{Context, Pos2, Rect, Vec2};

use std::sync::{Arc, Mutex};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};

static ICON: &[u8] = include_bytes!("../../assets/tray-template.png");

/// What was picked in the menu since the last frame.
#[derive(Debug, Clone, PartialEq)]
pub enum TrayEvent {
    Open,
    Floating,
}

/// The wall display's part of the menu: its switch and, while it's on, the
/// address to open on the other screen (or why it couldn't start).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WallMenu {
    pub on: bool,
    pub lines: Vec<String>,
    /// offered by "Copy link" when there's one
    pub link: Option<String>,
}

pub struct Tray {
    icon: TrayIcon,
    title: String,
    /// the rows at the top of the menu, as last shown
    rows: Vec<Row>,
    /// the fine print in the Details submenu, as last shown
    details: Vec<String>,
    wall_menu: WallMenu,
    open: MenuItem,
    floating: MenuItem,
    wall: CheckMenuItem,
    copy: MenuItem,
    quit: MenuItem,
    events: Arc<Mutex<Vec<TrayEvent>>>,
}

/// How close a limit is to running out; picks the row's color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tone {
    Calm,
    Warn,
    Hot,
}

impl Tone {
    /// `warn` and `fire` are the app's own alert and fire thresholds, so the
    /// menu agrees with the 🔥 in the menu bar and with notifications.
    pub fn of(pct: Option<f64>, warn: f64, fire: f64) -> Tone {
        match pct {
            Some(p) if p >= fire => Tone::Hot,
            Some(p) if p >= warn => Tone::Warn,
            _ => Tone::Calm,
        }
    }
}

/// One informational row at the top of the menu. Rows hold finished text, not
/// raw milliseconds, so two frames that read the same compare equal and the
/// menu is only rebuilt when what it shows changes.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// A section title ("Claude"); macOS only, other platforms skip it.
    Header(String),
    /// A limit: "5h" under "Claude", its percent, and when it resets.
    Meter { group: String, label: String, pct: Option<f64>, clock: Option<String>, left: Option<String>, tone: Tone },
    /// Anything else, shown as is.
    Text(String),
}

impl Row {
    /// A limit row. `reset_ms` is the time left; `countdown` adds it beside
    /// the clock time (worth it on the 5h window, noise on a weekly one).
    pub fn meter(group: &str, label: &str, pct: Option<f64>, reset_ms: Option<i64>, countdown: bool, warn: f64, fire: f64) -> Row {
        let live = pct.and(reset_ms);
        Row::Meter {
            group: group.into(),
            label: label.into(),
            pct,
            clock: live.map(crate::format::clock),
            left: live.filter(|_| countdown).map(crate::format::countdown),
            tone: Tone::of(pct, warn, fire),
        }
    }

    /// The row as one plain line: what Windows and Linux show, and what a
    /// macOS item falls back to. A header has none there.
    pub fn plain(&self) -> Option<String> {
        match self {
            Row::Header(_) => None,
            Row::Text(t) => Some(t.clone()),
            Row::Meter { group, label, pct, clock, left, .. } => {
                let value = pct.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "—".into());
                let name = format!("{group} {label}");
                Some(match (clock, left) {
                    (Some(c), Some(l)) => format!("{name} {value} · resets {c} ({l})"),
                    (Some(c), None) => format!("{name} {value} · resets {c}"),
                    _ => format!("{name} {value}"),
                })
            }
        }
    }

    /// The macOS text for a limit: label, bar, percent and reset, in columns
    /// (the item is drawn in a monospaced font so they line up).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn columns(&self) -> Option<String> {
        let Row::Meter { label, pct, clock, left, .. } = self else { return None };
        let Some(p) = pct else { return Some(format!("{label:<6}—")) };
        let when = match (clock, left) {
            (Some(c), Some(l)) => format!("{l} · {c}"),
            (Some(c), None) => c.clone(),
            _ => String::new(),
        };
        Some(format!("{label:<6}{}  {p:>3.0}%  {when}", bar(*p)).trim_end().to_string())
    }
}

/// Ten cells; a limit that is used at all always shows one.
pub fn bar(pct: f64) -> String {
    let p = if pct.is_finite() { pct.clamp(0.0, 100.0) } else { 0.0 };
    let filled = ((p / 10.0).floor() as usize).max(usize::from(p > 0.0)).min(10);
    format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled))
}

/// What an item's text is drawn with on macOS.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ink {
    Label,
    Secondary,
    Orange,
    Red,
}

#[cfg(target_os = "macos")]
impl From<Tone> for Ink {
    fn from(t: Tone) -> Ink {
        match t {
            Tone::Calm => Ink::Label,
            Tone::Warn => Ink::Orange,
            Tone::Hot => Ink::Red,
        }
    }
}

// Informational rows are kept *enabled* on macOS: AppKit dims every disabled
// menu item (even with an attributed title) until it's hard to read over the
// translucent menu. They have no action, so a click just closes the menu (the
// event handler ignores unknown ids). Other platforms keep them disabled.
// The text gets an adaptive system color; limits are drawn monospaced.
#[cfg(target_os = "macos")]
fn styled_title(text: &str, ink: Ink, mono: bool) -> objc2::rc::Retained<objc2_foundation::NSAttributedString> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName};
    use objc2_foundation::{NSAttributedString, NSAttributedStringKey, NSDictionary, NSString};

    let string = NSString::from_str(text);
    let color = match ink {
        Ink::Label => NSColor::labelColor(),
        Ink::Secondary => NSColor::secondaryLabelColor(),
        Ink::Orange => NSColor::systemOrangeColor(),
        Ink::Red => NSColor::systemRedColor(),
    };
    // SAFETY: NSColor is an Objective-C object and can be erased to AnyObject.
    let color: Retained<AnyObject> = unsafe { Retained::cast_unchecked(color) };
    // SAFETY: AppKit exposes these stable global keys for attributed-string attributes.
    let color_key = unsafe { NSForegroundColorAttributeName };
    let attributes = if mono {
        let font = NSFont::monospacedSystemFontOfSize_weight(13.0, 0.0);
        // SAFETY: NSFont is an Objective-C object and can be erased to AnyObject.
        let font: Retained<AnyObject> = unsafe { Retained::cast_unchecked(font) };
        let font_key = unsafe { NSFontAttributeName };
        NSDictionary::<NSAttributedStringKey, AnyObject>::from_slices(&[color_key, font_key], &[&*color, &*font])
    } else {
        NSDictionary::<NSAttributedStringKey, AnyObject>::from_slices(&[color_key], &[&*color])
    };
    // SAFETY: the dictionary holds valid attribute keys and retained objects
    // for the full lifetime of the initializer.
    unsafe { NSAttributedString::new_with_attributes(&string, &attributes) }
}

/// An informational item: enabled on macOS so AppKit doesn't dim it.
fn status_item(text: &str) -> MenuItem {
    MenuItem::new(text, cfg!(target_os = "macos"), None)
}

/// The menu item for a row, or none where the platform skips it.
fn row_item(row: &Row) -> Option<MenuItem> {
    #[cfg(target_os = "macos")]
    {
        let (text, ink, mono) = match row {
            Row::Header(t) => (t.clone(), Ink::Secondary, false),
            Row::Text(t) => (t.clone(), Ink::Label, false),
            Row::Meter { tone, .. } => (row.columns()?, Ink::from(*tone), true),
        };
        let item = status_item(&text);
        item.set_attributed_title(Some(&styled_title(&text, ink, mono)));
        Some(item)
    }
    #[cfg(not(target_os = "macos"))]
    {
        row.plain().map(|t| status_item(&t))
    }
}

// the menu: the limit rows (read-only), the fine print, then the actions
/// The menu's action items, built once so their ids stay put.
struct Actions<'a> {
    open: &'a MenuItem,
    floating: &'a MenuItem,
    wall: &'a CheckMenuItem,
    copy: &'a MenuItem,
    quit: &'a MenuItem,
}

fn build_menu(rows: &[Row], details: &[String], wall: &WallMenu, a: &Actions) -> Option<Menu> {
    let menu = Menu::new();
    for r in rows {
        if let Some(item) = row_item(r) {
            menu.append(&item).ok()?;
        }
    }
    if !details.is_empty() {
        // the parent must be enabled, or it won't open
        let sub = Submenu::new("Details", true);
        for d in details {
            let item = status_item(d);
            #[cfg(target_os = "macos")]
            item.set_attributed_title(Some(&styled_title(d, Ink::Label, false)));
            sub.append(&item).ok()?;
        }
        menu.append(&sub).ok()?;
    }
    if !rows.is_empty() || !details.is_empty() {
        menu.append(&PredefinedMenuItem::separator()).ok()?;
    }
    menu.append_items(&[a.open, a.floating, &PredefinedMenuItem::separator()]).ok()?;
    a.wall.set_checked(wall.on);
    menu.append(a.wall).ok()?;
    for l in &wall.lines {
        let item = status_item(l);
        #[cfg(target_os = "macos")]
        item.set_attributed_title(Some(&styled_title(l, Ink::Secondary, false)));
        menu.append(&item).ok()?;
    }
    if wall.link.is_some() {
        menu.append(a.copy).ok()?;
    }
    menu.append_items(&[&PredefinedMenuItem::separator(), a.quit]).ok()?;
    Some(menu)
}

fn decode(bytes: &[u8]) -> Option<tray_icon::Icon> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut r = dec.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    tray_icon::Icon::from_rgba(buf[..info.buffer_size()].to_vec(), info.width, info.height).ok()
}

impl Tray {
    /// Put the icon in the menu bar. Call on the main thread.
    pub fn new(ctx: &Context) -> Option<Self> {
        let events: Arc<Mutex<Vec<TrayEvent>>> = Arc::default();
        let (open, floating, quit) =
            (MenuItem::new("Open TokenRunway", true, None), MenuItem::new("Floating only", true, None), MenuItem::new("Quit TokenRunway", true, None));
        let wall = CheckMenuItem::new("Wall display (iPad, other screens)", true, false, None);
        let copy = MenuItem::new("Copy wall display link", true, None);
        let menu = build_menu(&[], &[], &WallMenu::default(), &Actions { open: &open, floating: &floating, wall: &wall, copy: &copy, quit: &quit })?;
        let builder = TrayIconBuilder::new();
        // macOS tints a template icon for the menu bar's theme
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(decode(ICON)?);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(decode(ICON)?);
        let icon = builder
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .with_tooltip("TokenRunway")
            // Keep the saved menu-bar position independent of upstream Clauddy.
            .with_autosave_name("tokenrunway")
            .build()
            .ok()?;
        let (q, c) = (events.clone(), ctx.clone());
        let ids = (open.id().clone(), floating.id().clone(), quit.id().clone(), wall.id().clone(), copy.id().clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let ev = if e.id == ids.0 {
                TrayEvent::Open
            } else if e.id == ids.1 {
                TrayEvent::Floating
            } else if e.id == ids.2 {
                // no frame may come to answer it while the window is hidden
                std::process::exit(0);
            } else if e.id == ids.3 || e.id == ids.4 {
                // handled here, not in a frame: a hidden window gets none,
                // and the server needs no window to start or stop
                if e.id == ids.3 {
                    crate::web::toggle();
                } else {
                    crate::web::copy_link();
                }
                c.request_repaint();
                return;
            } else {
                return;
            };
            // a hidden window gets no frames: show it so the next one
            // answers, once the menu has closed
            crate::platform::wake_after_menu(c.clone());
            if let Ok(mut v) = q.lock() {
                v.push(ev);
            }
            c.request_repaint();
        }));
        Some(Tray { icon, title: String::new(), rows: vec![], details: vec![], wall_menu: WallMenu::default(), open, floating, wall, copy, quit, events })
    }

    /// The rows at the top of the menu and the fine print under Details;
    /// rebuilt only when what they show changes.
    pub fn set_rows(&mut self, rows: Vec<Row>, details: Vec<String>, wall: WallMenu) {
        if rows == self.rows && details == self.details && wall == self.wall_menu {
            return;
        }
        let actions = Actions { open: &self.open, floating: &self.floating, wall: &self.wall, copy: &self.copy, quit: &self.quit };
        if let Some(menu) = build_menu(&rows, &details, &wall, &actions) {
            self.icon.set_menu(Some(Box::new(menu)));
            self.rows = rows;
            self.details = details;
            self.wall_menu = wall;
        }
    }

    /// The icon's box, in physical pixels, where the OS says.
    pub fn rect(&self) -> Option<Rect> {
        self.icon.rect().map(|r| Rect::from_min_size(Pos2::new(r.position.x as f32, r.position.y as f32), Vec2::new(r.size.width as f32, r.size.height as f32)))
    }

    pub fn take_events(&self) -> Vec<TrayEvent> {
        self.events.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
    }

    /// The text beside the icon (macOS) or in its tooltip; set only on change.
    pub fn set_title(&mut self, title: &str) {
        if title == self.title {
            return;
        }
        self.title = title.to_string();
        #[cfg(target_os = "macos")]
        self.icon.set_title((!title.is_empty()).then(|| format!(" {title}")));
        let _ = self.icon.set_tooltip(Some(if title.is_empty() { "TokenRunway".to_string() } else { format!("TokenRunway · {title}") }));
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
    }
}

/// The menu bar's text: the session % (🔥 past the fire threshold), or
/// nothing while there's no reading.
pub fn title(session_pct: Option<f64>, fire: f64) -> String {
    match session_pct {
        Some(p) if p >= fire => format!("🔥 {p:.0}%"),
        Some(p) => format!("{p:.0}%"),
        None => String::new(),
    }
}

/// Several services: one number, the session closest to its limit. A
/// title per service grew wide enough to land behind the notch, where
/// macOS hides it; the menu lists each one.
pub fn title_hottest(sessions: &[Option<f64>], fire: f64) -> String {
    title(sessions.iter().flatten().copied().reduce(f64::max), fire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn several_services_show_the_hottest() {
        assert_eq!(super::title_hottest(&[Some(64.0), Some(28.0), None], 90.0), "64%");
        assert_eq!(super::title_hottest(&[Some(20.0), Some(93.0)], 90.0), "🔥 93%");
        assert_eq!(super::title_hottest(&[None, None], 90.0), "");
    }

    #[test]
    fn the_menu_bar_reads_the_session_and_catches_fire() {
        assert_eq!(super::title(Some(42.4), 90.0), "42%");
        assert_eq!(super::title(Some(93.0), 90.0), "🔥 93%");
        assert_eq!(super::title(None, 90.0), "");
    }

    fn limit(pct: Option<f64>, ms: Option<i64>, countdown: bool) -> Row {
        Row::meter("Claude", "5h", pct, ms, countdown, 80.0, 90.0)
    }

    #[test]
    fn a_limit_line_names_its_reset() {
        assert!(limit(Some(39.2), Some(60_000), false).plain().unwrap().starts_with("Claude 5h 39% · resets "));
        assert_eq!(Row::meter("Claude", "weekly", None, Some(60_000), false, 80.0, 90.0).plain().as_deref(), Some("Claude weekly —"));
        assert!(limit(Some(35.0), Some(60_000), true).plain().unwrap().ends_with("(1m)"));
    }

    #[test]
    fn headers_are_macos_only_and_text_rows_pass_through() {
        assert_eq!(Row::Header("Claude".into()).plain(), None);
        assert_eq!(Row::Text("hello".into()).plain().as_deref(), Some("hello"));
    }

    #[test]
    fn the_bar_fills_by_tens_and_never_hides_use() {
        assert_eq!(bar(0.0), "░░░░░░░░░░");
        assert_eq!(bar(0.4), "█░░░░░░░░░");
        assert_eq!(bar(35.0), "███░░░░░░░");
        assert_eq!(bar(99.6), "█████████░");
        assert_eq!(bar(100.0), "██████████");
        assert_eq!(bar(250.0), "██████████");
        assert_eq!(bar(-5.0), "░░░░░░░░░░");
        assert_eq!(bar(f64::NAN), "░░░░░░░░░░");
    }

    #[test]
    fn tone_follows_the_alert_and_fire_thresholds() {
        assert_eq!(Tone::of(Some(79.0), 80.0, 90.0), Tone::Calm);
        assert_eq!(Tone::of(Some(80.0), 80.0, 90.0), Tone::Warn);
        assert_eq!(Tone::of(Some(89.0), 80.0, 90.0), Tone::Warn);
        assert_eq!(Tone::of(Some(90.0), 80.0, 90.0), Tone::Hot);
        assert_eq!(Tone::of(None, 80.0, 90.0), Tone::Calm);
    }

    #[test]
    fn columns_line_up_label_bar_percent_and_reset() {
        let row = limit(Some(35.0), Some(4 * 3_600_000 + 27 * 60_000), true);
        let text = row.columns().unwrap();
        assert!(text.starts_with("5h    ███░░░░░░░   35%  4h 27m · "), "{text}");
        let unavailable = Row::meter("Claude", "Week", None, None, false, 80.0, 90.0);
        assert_eq!(unavailable.columns().as_deref(), Some("Week  —"));
    }

    #[test]
    fn the_template_icon_decodes() {
        assert!(super::decode(super::ICON).is_some());
    }
}
