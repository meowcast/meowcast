//! Store: the device's app store. It lists the apps of every repository, installs, opens and
//! removes them, and keeps the list of repositories: the meow store, which is built in, and
//! any the user adds by address.
//!
//! The app itself has no network access and installs nothing: the system (appd) fetches and
//! checks everything, and this app asks it over the broker (`System::store`, which needs the
//! "manage-apps" permission). What it adds is the screens, including the permission question
//! before an install, whose answer appd records.
//!
//!   store ui-preview OUT_DIR [FONT_DIR]   render every screen to a PNG on a desk
//!   store selftest REQUEST...             (as an app without a screen, test-store.sh) print
//!                                         every line the system sends and make the requests

mod model;
mod state;
mod ui;

use std::time::{Duration, Instant};

use meow_app::{App, Event, Key};
use meow_media::canvas::Canvas;
use meow_media::video::{Display, Edge, Frame, Placement, Rect};

use model::{Catalog, Update};
use state::{Action, Layer, State, Tab};

const FONTS: &str = "/usr/share/fonts/noto";
/// Frames that take longer than this are logged.
const SLOW_FRAME_MS: u64 = 200;

/// Put the canvas on the screen: only the part that differs from what is there.
fn present(display: &Display, canvas: &Canvas, shown: &mut Vec<u32>) -> Result<(), String> {
    let screen = Rect::new(0, 0, canvas.width as u32, canvas.height as u32);
    let damage = if shown.len() == canvas.pixels.len() {
        match ui::changed(shown, &canvas.pixels, canvas.width) {
            Some((x, y, width, height)) => Rect::new(x as u32, y as u32, width as u32, height as u32),
            None => return Ok(()),
        }
    } else {
        screen
    };
    let placement = Placement { area: screen, from: Edge::None, travel: 0, duration: Duration::ZERO };
    display.publish_painted(placement, &[damage], false, &mut |frame: &mut Frame, part: Option<Rect>| {
        if let Some(part) = part {
            frame.copy(canvas, part);
        }
    })?;
    shown.clone_from(&canvas.pixels);
    Ok(())
}

/// The canvas at half size as a PPM file (`appctl key shot`, then read
/// /proc/PID/root/tmp/shot.ppm): for looking at the app on a device without a camera.
fn save_shot(canvas: &Canvas) {
    let (width, height) = (canvas.width / 2, canvas.height / 2);
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    for y in 0..height {
        for x in 0..width {
            let pixel = canvas.pixels[y * 2 * canvas.width + x * 2];
            out.extend([(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
        }
    }
    match std::fs::write("/tmp/shot.ppm", out) {
        Ok(()) => println!("store: screenshot /tmp/shot.ppm"),
        Err(e) => println!("store: screenshot: {e}"),
    }
}

fn run(mut app: App) -> Result<(), String> {
    if !app.has("manage-apps") {
        return Err("the manage-apps permission is not granted: this app cannot be the store".into());
    }
    let card = app.display_fd().map_err(|e| format!("display: {e}"))?;
    let display = Display::open(Some(card))?;
    let fonts = ui::Fonts::load(FONTS)?;
    let mut canvas = Canvas::new(display.width as usize, display.height as usize);
    ui::draw_background(&mut canvas);
    let background = canvas.pixels.clone();
    let mut osd = meow_app::osd::Renderer::from_font_dir(FONTS, display.height).inspect_err(|e| eprintln!("store: volume pop-up: {e}")).ok();

    let system = app.system();
    system.store(&["subscribe"]);
    let mut catalog = Catalog::default();
    let mut state = State::new(&app.id);
    let (mut shown, mut dirty, mut popup_shown) = (Vec::new(), true, None);
    loop {
        for event in app.wait(Some(Duration::from_millis(100))) {
            match event {
                Event::Quit => return Ok(()),
                Event::Key(Key::Other(key)) if key == "shot" => save_shot(&canvas),
                Event::Key(key) => {
                    for action in state.key(&key, &catalog) {
                        match action {
                            Action::Exit => {
                                println!("store: left with Back");
                                return Ok(());
                            }
                            Action::Send(fields) => system.store(&fields.iter().map(String::as_str).collect::<Vec<_>>()),
                        }
                    }
                    dirty = true;
                }
                Event::Store(line) => match catalog.apply(&line) {
                    Update::Changed => {
                        if line == "end" {
                            println!("store: {} apps from {} repositories", catalog.apps.len(), catalog.repos.len());
                        }
                        state.settle(&catalog);
                        dirty = true;
                    }
                    Update::Result { what, target, ok, text } => {
                        state.result(&what, &target, ok, &text, &catalog);
                        dirty = true;
                    }
                    Update::Nothing => {}
                },
                _ => {}
            }
        }
        dirty |= state.expire(Instant::now());
        // The system's volume pop-up, drawn by whoever has the screen.
        let popup = osd.as_ref().and_then(|_| meow_app::osd::current()).map(|p| (p.level, p.muted));
        dirty |= popup != popup_shown;
        if !dirty {
            continue;
        }
        let began = Instant::now();
        ui::draw(&mut canvas, &fonts, &state, &catalog, Some(&background));
        if let (Some((level, muted)), Some(renderer)) = (popup, osd.as_mut()) {
            let (x, y, _, _) = meow_app::osd::place(canvas.width as u32, canvas.height as u32);
            renderer.render(level, muted).blend_into(&mut canvas.pixels, canvas.width, x, y, 1.0);
        }
        let drawn = began.elapsed();
        present(&display, &canvas, &mut shown)?;
        // A slow frame is worth a line in the log (`appctl logs`); a usual one takes about
        // 40 ms to draw and is on screen 100 ms after the key (15 Nov, at 600 MHz).
        if began.elapsed() > Duration::from_millis(SLOW_FRAME_MS) {
            println!("store: slow frame: drawn in {} ms, on screen after {} ms", drawn.as_millis(), began.elapsed().as_millis());
        }
        (dirty, popup_shown) = (false, popup);
    }
}

/// The broker side without a screen, for test-store.sh: print every line of the system, and
/// once the first list is complete make `requests` one after another, each when the one
/// before was answered. A request is its fields joined by '|'; "@key" stands for the
/// fingerprint the last look-up returned.
fn selftest(mut app: App, requests: &[String]) -> Result<(), String> {
    let system = app.system();
    system.store(&["subscribe"]);
    let (mut next, mut ready, mut refreshing, mut fingerprint) = (0, false, false, String::new());
    let mut quiet_since = Instant::now();
    loop {
        for event in app.wait(Some(Duration::from_millis(100))) {
            match event {
                Event::Quit => return Ok(()),
                Event::Store(line) => {
                    println!("STORE {}", line.replace('\t', " | "));
                    let fields: Vec<&str> = line.split('\t').collect();
                    ready |= matches!(fields[0], "end" | "result");
                    quiet_since = Instant::now();
                    if let ["result", "probe", _, "ok", _, key, ..] = fields[..] {
                        fingerprint = key.to_string();
                    }
                }
                _ => {}
            }
        }
        if ready && next < requests.len() {
            let fields: Vec<&str> = requests[next].split('|').map(|f| if f == "@key" { fingerprint.as_str() } else { f }).collect();
            println!("REQUEST {}", fields.join(" | "));
            system.store(&fields);
            // "refresh" has no answer: the lists that follow are, and then two quiet seconds.
            (ready, refreshing) = (false, fields[0] == "refresh");
            (next, quiet_since) = (next + 1, Instant::now());
        } else if refreshing && quiet_since.elapsed() > Duration::from_secs(2) {
            (ready, refreshing) = (true, false);
        } else if next >= requests.len() && ready && quiet_since.elapsed() > Duration::from_secs(3) {
            return Ok(());
        }
    }
}

/// Every screen as a picture, from the sample catalogue.
fn ui_preview(out: &str, fonts: &str) -> Result<(), String> {
    let fonts = ui::Fonts::load(fonts)?;
    let catalog = model::sample();
    let own = "dev.meow.store";
    let base = State::new(own);
    let with = |change: &dyn Fn(&mut State)| {
        let mut state = base.clone();
        change(&mut state);
        state
    };
    let mut many = catalog.clone();
    for index in 0..9 {
        let mut app = many.apps[index % 3].clone();
        app.id = format!("org.example.more{index}");
        app.name = format!("{} {}", app.name, index + 2);
        many.apps.push(app);
    }
    let mut broken = catalog.clone();
    broken.apply("repo\tr12345678\tExample apps\thttps://apps.example.org/tv/\t9f8e 7d6c 5b4a 3921\t1\t0\terror\tconnection timed out\t2");
    let empty = Catalog::default();
    let screens: Vec<(&str, State, &Catalog)> = vec![
        ("browse", with(&|s| s.app_sel = 1), &catalog),
        ("browse-scrolled", with(&|s| (s.app_sel, s.scroll) = (13, 2)), &many),
        ("installed", with(&|s| (s.tab, s.in_tabs) = (Tab::Installed, true)), &catalog),
        ("loading", base.clone(), &empty),
        ("detail-available", with(&|s| s.layer = Layer::Detail { id: "dev.meow.cast".into(), button: 0 }), &catalog),
        ("detail-installed", with(&|s| s.layer = Layer::Detail { id: "dev.meow.youtube".into(), button: 1 }), &catalog),
        ("detail-self", with(&|s| s.layer = Layer::Detail { id: own.into(), button: 0 }), &catalog),
        ("consent", with(&|s| s.layer = Layer::Consent { id: "dev.meow.cast".into(), button: 0 }), &catalog),
        ("uninstall", with(&|s| s.layer = Layer::Uninstall { id: "org.example.radio".into(), button: 1 }), &catalog),
        ("repositories", with(&|s| (s.tab, s.source_sel) = (Tab::Sources, 1)), &broken),
        ("repository", with(&|s| (s.tab, s.layer) = (Tab::Sources, Layer::RepoMenu { repo: "r12345678".into(), button: 0 })), &broken),
        ("repository-remove", with(&|s| (s.tab, s.layer) = (Tab::Sources, Layer::RemoveRepo { repo: "r12345678".into(), button: 1 })), &catalog),
        ("add", with(&|s| s.layer = Layer::AddUrl { text: "https://apps.example.org/tv".into(), row: 3, col: 8, busy: false, error: String::new() }), &catalog),
        ("add-error", with(&|s| s.layer = Layer::AddUrl { text: "https://apps.example.org/tv".into(), row: 4, col: 9, busy: false, error: "No app repository at this address (HTTP 404)".into() }), &catalog),
        ("add-confirm", with(&|s| s.layer = Layer::ConfirmAdd { url: "https://apps.example.org/tv/".into(), name: "Example apps".into(), fingerprint: "9f8e 7d6c 5b4a 3921".into(), apps: 2, button: 1 }), &catalog),
        ("notice", with(&|s| {
            s.layer = Layer::Detail { id: "dev.meow.inspector".into(), button: 0 };
            s.say("Sandbox Inspector is installed");
        }), &catalog),
    ];
    std::fs::create_dir_all(out).map_err(|e| format!("{out}: {e}"))?;
    let mut canvas = Canvas::new(1280, 720);
    for (name, state, catalog) in screens {
        ui::draw(&mut canvas, &fonts, &state, catalog, None);
        let path = format!("{out}/store-{name}.png");
        std::fs::write(&path, canvas.png()).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("ui-preview") if args.len() >= 3 => ui_preview(&args[2], args.get(3).map_or(FONTS, String::as_str)),
        Some("selftest") => App::connect().map_err(|e| e.to_string()).and_then(|app| selftest(app, &args[2..])),
        _ => match App::connect() {
            Ok(app) => run(app),
            Err(e) => Err(format!("{e} (the Store is started by appd; `store ui-preview OUT_DIR [FONT_DIR]` works anywhere)")),
        },
    };
    if let Err(e) = result {
        eprintln!("store: {e}");
        std::process::exit(1);
    }
}
