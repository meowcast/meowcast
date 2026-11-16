//! The Store's screens: pure drawing from the catalogue and the navigation state into the
//! shared canvas, so every screen can be rendered to a PNG on a desk (`store ui-preview DIR`).
//!
//! Layout numbers are for a 1080-line screen and scale with the height, as in the launcher.

use std::cell::RefCell;
use std::collections::HashMap;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use meow_media::canvas::{Canvas, Color, fit, measure, rgba};

use crate::model::{AppEntry, Catalog, Repo};
use crate::state::{COLUMNS, KEYS, Layer, ROWS_SHOWN, State, TABS, Tab};

const WHITE: Color = 0xffff_ffff;
const MUTED: Color = rgba(0xb9c1cd, 255);
const FAINT: Color = rgba(0xb9c1cd, 150);
const ACCENT: Color = rgba(0x7c9cff, 255);
const GOOD: Color = rgba(0x4ad06a, 255);
const WARN: Color = rgba(0xf3b04c, 255);
const CARD: Color = rgba(0x1a2031, 255);
const CARD_FOCUS: Color = rgba(0x2a3350, 255);
const DARK: Color = rgba(0x0b0e14, 255);

/// A glyph drawn once: its coverage and where it sits relative to the pen on the baseline.
struct Raster {
    left: i32,
    top: i32,
    width: usize,
    coverage: Vec<u8>,
}

/// A font with the glyphs it has drawn so far. Working out a glyph's outline is what text
/// costs on this CPU; a screen repeats the same few hundred glyphs on every key press.
pub struct Face {
    font: FontVec,
    drawn: RefCell<HashMap<(char, u32), Option<Raster>>>,
}

impl std::ops::Deref for Face {
    type Target = FontVec;
    fn deref(&self) -> &FontVec {
        &self.font
    }
}

/// One line of text with its baseline at `baseline`; returns the width drawn. As the
/// canvas's own, but from the kept glyphs, each placed on a whole pixel.
fn draw_text(canvas: &mut Canvas, face: &Face, size: f32, x: f32, baseline: f32, color: Color, line: &str) -> f32 {
    let scaled = face.font.as_scaled(PxScale::from(size));
    let mut drawn = face.drawn.borrow_mut();
    let (mut pen, mut previous) = (x, None);
    for character in line.chars() {
        let id = scaled.glyph_id(character);
        if id.0 == 0 {
            continue;
        }
        if let Some(previous) = previous {
            pen += scaled.kern(previous, id);
        }
        let raster = drawn.entry((character, (size * 16.0) as u32)).or_insert_with(|| {
            let outline = face.font.outline_glyph(id.with_scale_and_position(PxScale::from(size), ab_glyph::point(0.0, 0.0)))?;
            let bounds = outline.px_bounds();
            let (width, height) = (bounds.width().ceil() as usize + 1, bounds.height().ceil() as usize + 1);
            let mut coverage = vec![0u8; width * height];
            outline.draw(|gx, gy, c| {
                if let Some(cell) = coverage.get_mut(gy as usize * width + gx as usize) {
                    *cell = (c * 255.0) as u8;
                }
            });
            Some(Raster { left: bounds.min.x.floor() as i32, top: bounds.min.y.floor() as i32, width, coverage })
        });
        if let Some(raster) = raster {
            let (x0, y0) = (pen.round() as i32 + raster.left, baseline.round() as i32 + raster.top);
            for (index, cover) in raster.coverage.iter().enumerate().filter(|(_, cover)| **cover > 0) {
                let (px, py) = (x0 + (index % raster.width) as i32, y0 + (index / raster.width) as i32);
                if px >= 0 && py >= 0 && (px as usize) < canvas.width && (py as usize) < canvas.height {
                    blend(&mut canvas.pixels[py as usize * canvas.width + px as usize], color, *cover as u32 * 256 / 255);
                }
            }
        }
        pen += scaled.h_advance(id);
        previous = Some(id);
    }
    pen - x
}

pub struct Fonts {
    light: Face,
    regular: Face,
    medium: Face,
}

impl Fonts {
    pub fn load(directory: &str) -> Result<Fonts, String> {
        let face = |name: &str| {
            let path = format!("{directory}/NotoSans-{name}.ttf");
            let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
            let font = FontVec::try_from_vec(bytes).map_err(|e| format!("{path}: {e}"))?;
            Ok::<Face, String>(Face { font, drawn: RefCell::default() })
        };
        Ok(Fonts { light: face("Light")?, regular: face("Regular")?, medium: face("Medium")? })
    }
}

/// Source-over of a straight colour at `coverage` (0..=256) onto a premultiplied pixel.
fn blend(pixel: &mut u32, color: Color, coverage: u32) {
    let alpha = (color >> 24) * coverage >> 8;
    if alpha == 0 {
        return;
    }
    let keep = 255 - alpha;
    let channel = |shift: u32| (((color >> shift & 0xff) * alpha + 127) / 255 + ((*pixel >> shift & 0xff) * keep + 127) / 255).min(255);
    *pixel = (alpha + ((*pixel >> 24) * keep + 127) / 255).min(255) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0);
}

/// A flat rounded rectangle, filled a row at a time: the corners cost one square root per
/// row and the inside of an opaque one is a plain fill. (The canvas's own rounded rectangle
/// works out a distance per pixel, which is too slow for a screen full of cards on this CPU.)
fn panel(canvas: &mut Canvas, x: f32, y: f32, width: f32, height: f32, radius: f32, color: Color) {
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    let opaque = color >> 24 == 255;
    let solid = 0xff00_0000 | (color & 0xff_ffff);
    for py in (y.floor() as i32).max(0)..((y + height).ceil() as i32).min(canvas.height as i32) {
        let centre = py as f32 + 0.5;
        // How far this row is into a corner's curve, and the row's own coverage at the edge.
        let into = (y + radius - centre).max(centre - (y + height - radius)).max(0.0);
        let inset = radius - (radius * radius - into * into).max(0.0).sqrt();
        let vertical = ((centre - y + 0.5).min(y + height - centre + 0.5)).clamp(0.0, 1.0);
        let (from, to) = (x + inset, x + width - inset);
        let row = &mut canvas.pixels[py as usize * canvas.width..(py as usize + 1) * canvas.width];
        // The two edge pixels are partly covered; everything between them is one fill.
        let (first, last) = ((from.floor() as i32).max(0), (to.ceil() as i32).min(row.len() as i32));
        let (inner_from, inner_to) = ((from.ceil() as i32).clamp(first, last.max(first)), (to.floor() as i32).clamp(first, last.max(first)));
        let edge = |row: &mut [u32], px: i32| {
            let cover = ((px as f32 + 1.0).min(to) - (px as f32).max(from)).clamp(0.0, 1.0) * vertical;
            blend(&mut row[px as usize], color, (cover * 256.0) as u32);
        };
        if inner_to <= inner_from {
            (first..last).for_each(|px| edge(row, px));
            continue;
        }
        (first..inner_from).for_each(|px| edge(row, px));
        (inner_to..last).for_each(|px| edge(row, px));
        let inside = &mut row[inner_from as usize..inner_to as usize];
        if opaque && vertical >= 1.0 {
            inside.fill(solid);
        } else {
            let cover = (vertical * 256.0) as u32;
            inside.iter_mut().for_each(|pixel| blend(pixel, color, cover));
        }
    }
}

/// Darken everything (behind a dialog).
fn dim(canvas: &mut Canvas) {
    for pixel in &mut canvas.pixels {
        let half = |shift: u32| ((*pixel >> shift & 0xff) * 90 >> 8) << shift;
        *pixel = 0xff00_0000 | half(16) | half(8) | half(0);
    }
}

fn right(canvas: &mut Canvas, font: &Face, size: f32, right_edge: f32, baseline: f32, color: Color, text: &str) {
    draw_text(canvas, font, size, right_edge - measure(font, size, text), baseline, color, text);
}

fn centred(canvas: &mut Canvas, font: &Face, size: f32, centre: f32, baseline: f32, color: Color, text: &str) {
    draw_text(canvas, font, size, centre - measure(font, size, text) / 2.0, baseline, color, text);
}

/// Break `text` into at most `lines` lines of `width`.
fn wrap(font: &FontVec, size: f32, text: &str, width: f32, lines: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if measure(font, size, &candidate) <= width || line.is_empty() {
            line = candidate;
        } else {
            out.push(std::mem::replace(&mut line, word.to_string()));
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.len() > lines {
        let rest = out.split_off(lines - 1).join(" ");
        out.push(fit(font, size, &rest, width));
    }
    out.iter().map(|l| fit(font, size, l, width)).collect()
}

/// A row of buttons starting at `x`; the focused one is white.
fn buttons(canvas: &mut Canvas, fonts: &Fonts, s: f32, x: f32, y: f32, labels: &[&str], focused: Option<usize>) {
    let mut pen = x;
    for (index, label) in labels.iter().enumerate() {
        let width = measure(&fonts.medium, 28.0 * s, label) + 72.0 * s;
        let on = focused == Some(index);
        panel(canvas, pen, y, width, 64.0 * s, 32.0 * s, if on { WHITE } else { rgba(0xffffff, 36) });
        draw_text(canvas, &fonts.medium, 28.0 * s, pen + 36.0 * s, y + 42.0 * s, if on { DARK } else { WHITE }, label);
        pen += width + 20.0 * s;
    }
}

/// The app's badge: a disc in its two colours with its initial.
fn badge(canvas: &mut Canvas, fonts: &Fonts, x: f32, y: f32, size: f32, app: &AppEntry) {
    canvas.rounded_rect(x, y, size, size, size / 2.0, rgba(app.badge.0, 255), rgba(app.badge.1, 255));
    let initial: String = app.name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into());
    centred(canvas, &fonts.medium, size * 0.5, x + size / 2.0, y + size * 0.68, WHITE, &initial);
}

fn status_colour(app: &AppEntry) -> Color {
    match app.state.as_str() {
        "available" | "fetching" => ACCENT,
        "error" => WARN,
        _ if !app.detail.is_empty() => WARN,
        _ => GOOD,
    }
}

pub fn draw_background(canvas: &mut Canvas) {
    let scale = canvas.height as f32 / 1080.0;
    let (width, height) = (canvas.width as f32, canvas.height as f32);
    canvas.vertical_gradient(0, canvas.height, rgba(0x12151f, 255), rgba(0x07090d, 255));
    canvas.glow(width * 0.9, 0.02 * height, 900.0 * scale, rgba(0x5a5fff, 46));
    canvas.glow(width * 0.02, 1.0 * height, 800.0 * scale, rgba(0x2f8cff, 30));
}

/// Draw the whole screen. `background`: a kept copy of `draw_background` for this size
/// (it is slow to compute on the dongle).
pub fn draw(canvas: &mut Canvas, fonts: &Fonts, state: &State, catalog: &Catalog, background: Option<&[u32]>) {
    match background.filter(|b| b.len() == canvas.pixels.len()) {
        Some(kept) => canvas.pixels.copy_from_slice(kept),
        None => draw_background(canvas),
    }
    let s = canvas.height as f32 / 1080.0;
    match &state.layer {
        Layer::Detail { id, button } => detail(canvas, fonts, s, state, catalog, id, Some(*button)),
        Layer::Consent { id, .. } | Layer::Uninstall { id, .. } => detail(canvas, fonts, s, state, catalog, id, None),
        Layer::AddUrl { text, row, col, busy, error } => add_url(canvas, fonts, s, text, (*row, *col), *busy, error),
        Layer::ConfirmAdd { url, .. } => add_url(canvas, fonts, s, url, (usize::MAX, 0), false, ""),
        _ => browse(canvas, fonts, s, state, catalog),
    }
    match &state.layer {
        Layer::Consent { id, button } => {
            if let Some(app) = catalog.app(id) {
                let mut lines = vec![(format!("From {}. It asks for more than the basics:", catalog.repo_name(&app.repo)), MUTED)];
                for perm in app.sensitive().iter().take(6) {
                    lines.push((format!("\u{2022}  {}{}", perm.label, if perm.required { "" } else { " (optional)" }), WHITE));
                }
                lines.push(("\u{201c}Don't allow\u{201d} installs it without these, when the app can do without.".to_string(), FAINT));
                dialog(canvas, fonts, s, &format!("Install {}?", app.name), &lines, &["Allow", "Don't allow", "Cancel"], *button);
            }
        }
        Layer::Uninstall { id, button } => {
            if let Some(app) = catalog.app(id) {
                let lines = [("The app and everything it saved on this device will be removed.".to_string(), MUTED)];
                dialog(canvas, fonts, s, &format!("Uninstall {}?", app.name), &lines, &["Uninstall", "Cancel"], *button);
            }
        }
        Layer::RepoMenu { repo, button } => {
            if let Some(r) = catalog.repo(repo) {
                let mut lines = vec![(r.url.clone(), MUTED), (format!("Signing key  {}", r.fingerprint), MUTED)];
                lines.push(match r.state.as_str() {
                    "error" => (format!("Can't be reached: {}", r.detail), WARN),
                    "off" => ("Switched off: its apps are not offered, and installed ones get no updates.".to_string(), FAINT),
                    _ if r.builtin => ("Built in. The system's own parts come from here, so it stays on.".to_string(), FAINT),
                    _ => ("Its apps are checked against this key only; meow does not review them.".to_string(), FAINT),
                });
                dialog(canvas, fonts, s, &r.name, &lines, &State::repo_buttons(catalog, repo), *button);
            }
        }
        Layer::RemoveRepo { repo, button } => {
            if let Some(r) = catalog.repo(repo) {
                let installed = catalog.apps.iter().filter(|a| a.repo == *repo && a.installed()).count();
                let what = match installed {
                    0 => "Its apps will no longer be offered.".to_string(),
                    1 => "The app you installed from it will be removed with its data.".to_string(),
                    n => format!("The {n} apps you installed from it will be removed with their data."),
                };
                dialog(canvas, fonts, s, &format!("Remove {}?", r.name), &[(what, MUTED)], &["Remove", "Cancel"], *button);
            }
        }
        Layer::ConfirmAdd { url, name, fingerprint, apps, button } => {
            let lines = [
                (url.clone(), MUTED),
                (format!("{apps} app{} now. Signing key  {fingerprint}", if *apps == 1 { "" } else { "s" }), MUTED),
                ("Only add a repository you trust. meow does not review its apps; each one".to_string(), FAINT),
                ("still asks before it gets access to your network or its sites.".to_string(), FAINT),
            ];
            dialog(canvas, fonts, s, &format!("Add \u{201c}{name}\u{201d}?"), &lines, &["Add", "Cancel"], *button);
        }
        _ => {}
    }
    if let Some((text, _)) = &state.notice {
        let size = 28.0 * s;
        let text = fit(&fonts.regular, size, text, canvas.width as f32 - 400.0 * s);
        let width = measure(&fonts.regular, size, &text) + 80.0 * s;
        let (x, y) = ((canvas.width as f32 - width) / 2.0, canvas.height as f32 - 150.0 * s);
        panel(canvas, x, y, width, 68.0 * s, 34.0 * s, rgba(0xf2f4f8, 255));
        draw_text(canvas, &fonts.regular, size, x + 40.0 * s, y + 45.0 * s, DARK, &text);
    }
}

fn hint(canvas: &mut Canvas, fonts: &Fonts, s: f32, text: &str) {
    draw_text(canvas, &fonts.regular, 22.0 * s, 110.0 * s, canvas.height as f32 - 50.0 * s, FAINT, text);
}

fn browse(canvas: &mut Canvas, fonts: &Fonts, s: f32, state: &State, catalog: &Catalog) {
    let (left, width) = (110.0 * s, canvas.width as f32 - 220.0 * s);
    draw_text(canvas, &fonts.light, 72.0 * s, left, 150.0 * s, WHITE, "Store");
    let builtin = catalog.repos.iter().find(|r| r.builtin);
    let (summary, colour) = match (catalog.loaded, builtin) {
        (false, _) => ("Loading\u{2026}".to_string(), MUTED),
        (_, Some(r)) if r.state == "error" => (format!("The {} can't be reached", r.name), WARN),
        _ => {
            let (apps, repos) = (catalog.apps.len(), catalog.repos.iter().filter(|r| r.enabled).count());
            (format!("{apps} app{} from {repos} repositor{}", if apps == 1 { "" } else { "s" }, if repos == 1 { "y" } else { "ies" }), MUTED)
        }
    };
    right(canvas, &fonts.regular, 26.0 * s, left + width, 140.0 * s, colour, &summary);

    let mut pen = left;
    for (tab, label) in TABS {
        let tab_width = measure(&fonts.medium, 28.0 * s, label) + 56.0 * s;
        let (current, focused) = (tab == state.tab, tab == state.tab && state.in_tabs && state.layer == Layer::Browse);
        if current {
            panel(canvas, pen, 192.0 * s, tab_width, 60.0 * s, 30.0 * s, if focused { WHITE } else { rgba(0xffffff, 40) });
        }
        draw_text(canvas, &fonts.medium, 28.0 * s, pen + 28.0 * s, 232.0 * s, if focused { DARK } else if current { WHITE } else { MUTED }, label);
        pen += tab_width + 12.0 * s;
    }

    let top = 290.0 * s;
    if state.tab == Tab::Sources {
        sources(canvas, fonts, s, state, catalog, top);
        return hint(canvas, fonts, s, "OK opens a repository  \u{00b7}  Back leaves the Store");
    }
    let shown = state.shown(catalog);
    if shown.is_empty() {
        let text = match (catalog.loaded, state.tab) {
            (false, _) => "Loading the list of apps\u{2026}",
            (_, Tab::Installed) => "Nothing is installed yet. Look under All apps.",
            _ => "No apps to show. Check the internet connection in Settings.",
        };
        draw_text(canvas, &fonts.regular, 32.0 * s, left, top + 70.0 * s, MUTED, text);
        return hint(canvas, fonts, s, "Back leaves the Store");
    }
    let (gap, card_height) = (28.0 * s, 196.0 * s);
    let card_width = (width - gap * (COLUMNS as f32 - 1.0)) / COLUMNS as f32;
    for (index, app) in shown.iter().enumerate().skip(state.scroll * COLUMNS).take(ROWS_SHOWN * COLUMNS) {
        let (row, column) = (index / COLUMNS - state.scroll, index % COLUMNS);
        let (x, y) = (left + column as f32 * (card_width + gap), top + row as f32 * (card_height + gap));
        let focused = index == state.app_sel && !state.in_tabs;
        if focused {
            panel(canvas, x - 4.0 * s, y - 4.0 * s, card_width + 8.0 * s, card_height + 8.0 * s, 28.0 * s, WHITE);
        }
        panel(canvas, x, y, card_width, card_height, 24.0 * s, if focused { CARD_FOCUS } else { CARD });
        badge(canvas, fonts, x + 26.0 * s, y + 46.0 * s, 104.0 * s, app);
        let (text_x, text_width) = (x + 156.0 * s, card_width - 180.0 * s);
        draw_text(canvas, &fonts.medium, 30.0 * s, text_x, y + 66.0 * s, WHITE, &fit(&fonts.medium, 30.0 * s, &app.name, text_width));
        draw_text(canvas, &fonts.regular, 21.0 * s, text_x, y + 102.0 * s, FAINT, &fit(&fonts.regular, 21.0 * s, &app.summary, text_width));
        let from = catalog.repo(&app.repo).filter(|r| !r.builtin).map(|r| format!("  \u{00b7}  {}", r.name)).unwrap_or_default();
        let status = fit(&fonts.regular, 22.0 * s, &format!("{}{from}", app.status()), text_width);
        draw_text(canvas, &fonts.regular, 22.0 * s, text_x, y + 152.0 * s, status_colour(app), &status);
    }
    // More rows above or below: a small arrow at the list's edge.
    let (centre, rows) = (left + width / 2.0, shown.len().div_ceil(COLUMNS));
    if state.scroll > 0 {
        canvas.triangle([(centre - 14.0 * s, top - 12.0 * s), (centre + 14.0 * s, top - 12.0 * s), (centre, top - 26.0 * s)], FAINT);
    }
    if rows > state.scroll + ROWS_SHOWN {
        let y = top + ROWS_SHOWN as f32 * (card_height + gap) - 10.0 * s;
        canvas.triangle([(centre - 14.0 * s, y), (centre + 14.0 * s, y), (centre, y + 14.0 * s)], FAINT);
    }
    hint(canvas, fonts, s, "OK shows an app  \u{00b7}  Back leaves the Store");
}

fn sources(canvas: &mut Canvas, fonts: &Fonts, s: f32, state: &State, catalog: &Catalog, top: f32) {
    let (left, width, pitch) = (110.0 * s, canvas.width as f32 - 220.0 * s, 100.0 * s);
    let focus = |index: usize| index == state.source_sel && !state.in_tabs;
    let row = |canvas: &mut Canvas, index: usize| {
        let y = top + index as f32 * pitch;
        panel(canvas, left, y, width, pitch - 12.0 * s, 20.0 * s, if focus(index) { rgba(0xffffff, 44) } else { rgba(0xffffff, 14) });
        y
    };
    for (index, repo) in catalog.repos.iter().enumerate() {
        let y = row(canvas, index);
        let Repo { name, url, .. } = repo;
        let title = if repo.builtin { format!("{name}  \u{00b7}  built in") } else { name.clone() };
        draw_text(canvas, &fonts.medium, 29.0 * s, left + 32.0 * s, y + 38.0 * s, WHITE, &fit(&fonts.medium, 29.0 * s, &title, width - 420.0 * s));
        draw_text(canvas, &fonts.regular, 21.0 * s, left + 32.0 * s, y + 68.0 * s, FAINT, &fit(&fonts.regular, 21.0 * s, url, width - 420.0 * s));
        let colour = match repo.state.as_str() {
            "error" => WARN,
            "off" => FAINT,
            _ => MUTED,
        };
        right(canvas, &fonts.regular, 27.0 * s, left + width - 32.0 * s, y + 54.0 * s, colour, &repo.status());
    }
    let y = row(canvas, catalog.repos.len());
    draw_text(canvas, &fonts.medium, 29.0 * s, left + 32.0 * s, y + 38.0 * s, ACCENT, "Add a repository");
    draw_text(canvas, &fonts.regular, 21.0 * s, left + 32.0 * s, y + 68.0 * s, FAINT, "Apps from a web address you type. You see its signing key before it is added.");
}

fn detail(canvas: &mut Canvas, fonts: &Fonts, s: f32, state: &State, catalog: &Catalog, id: &str, focused: Option<usize>) {
    let Some(app) = catalog.app(id) else { return };
    let (left, width) = (110.0 * s, canvas.width as f32 - 220.0 * s);
    badge(canvas, fonts, left, 90.0 * s, 150.0 * s, app);
    draw_text(canvas, &fonts.light, 64.0 * s, left + 190.0 * s, 160.0 * s, WHITE, &fit(&fonts.light, 64.0 * s, &app.name, width - 190.0 * s));
    let publisher = if app.publisher.is_empty() { String::new() } else { format!("  \u{00b7}  {}", app.publisher) };
    draw_text(canvas, &fonts.regular, 26.0 * s, left + 190.0 * s, 204.0 * s, MUTED, &fit(&fonts.regular, 26.0 * s, &format!("Version {}{publisher}  \u{00b7}  {}", app.version, app.size()), width - 190.0 * s));
    draw_text(canvas, &fonts.regular, 24.0 * s, left + 190.0 * s, 240.0 * s, FAINT, &fit(&fonts.regular, 24.0 * s, &format!("From {}", catalog.repo_name(&app.repo)), width - 190.0 * s));
    draw_text(canvas, &fonts.regular, 32.0 * s, left, 322.0 * s, WHITE, &fit(&fonts.regular, 32.0 * s, &app.summary, width));
    draw_text(canvas, &fonts.regular, 26.0 * s, left, 368.0 * s, status_colour(app), &fit(&fonts.regular, 26.0 * s, &app.status(), width));

    let labels: Vec<&str> = state.buttons(app).iter().map(|b| b.label()).collect();
    if labels.is_empty() {
        draw_text(canvas, &fonts.regular, 26.0 * s, left, 446.0 * s, FAINT, "You are using this app.");
    } else {
        buttons(canvas, fonts, s, left, 404.0 * s, &labels, focused);
    }

    draw_text(canvas, &fonts.medium, 26.0 * s, left, 536.0 * s, MUTED, if app.installed() { "What it may do" } else { "What it asks for" });
    let (top, pitch) = (560.0 * s, 64.0 * s);
    let room = ((canvas.height as f32 - 80.0 * s - top) / pitch) as usize;
    for (index, perm) in app.perms.iter().enumerate().take(room) {
        let y = top + index as f32 * pitch;
        if index + 1 == room && app.perms.len() > room {
            draw_text(canvas, &fonts.regular, 24.0 * s, left + 34.0 * s, y + 30.0 * s, FAINT, &format!("and {} more", app.perms.len() - index));
            break;
        }
        let dot = if perm.sensitive { WARN } else { rgba(0xb9c1cd, 120) };
        panel(canvas, left, y + 14.0 * s, 14.0 * s, 14.0 * s, 7.0 * s, dot);
        let answer = match (perm.sensitive, perm.state.as_str()) {
            (true, "granted") => ("Allowed", GOOD),
            (true, "denied") => ("Not allowed", MUTED),
            (true, "undecided") => ("Not asked yet", MUTED),
            (true, _) if !perm.required => ("Optional", FAINT),
            _ => ("", FAINT),
        };
        right(canvas, &fonts.regular, 24.0 * s, left + width, y + 30.0 * s, answer.1, answer.0);
        draw_text(canvas, &fonts.regular, 26.0 * s, left + 34.0 * s, y + 28.0 * s, WHITE, &fit(&fonts.regular, 26.0 * s, &perm.label, width - 300.0 * s));
        draw_text(canvas, &fonts.regular, 20.0 * s, left + 34.0 * s, y + 54.0 * s, FAINT, &fit(&fonts.regular, 20.0 * s, &perm.reason, width - 300.0 * s));
    }
    hint(canvas, fonts, s, "Back returns to the list");
}

fn add_url(canvas: &mut Canvas, fonts: &Fonts, s: f32, text: &str, focus: (usize, usize), busy: bool, error: &str) {
    let (left, width) = (110.0 * s, canvas.width as f32 - 220.0 * s);
    draw_text(canvas, &fonts.light, 60.0 * s, left, 140.0 * s, WHITE, "Add a repository");
    draw_text(canvas, &fonts.regular, 26.0 * s, left, 190.0 * s, MUTED, "Type the web address of an app repository for this device.");
    panel(canvas, left, 226.0 * s, width, 84.0 * s, 20.0 * s, rgba(0xffffff, 30));
    // The end of a long address stays in view.
    let size = 36.0 * s;
    let mut shown: String = text.to_string();
    while measure(&fonts.regular, size, &shown) > width - 90.0 * s && !shown.is_empty() {
        shown.remove(0);
    }
    let end = draw_text(canvas, &fonts.regular, size, left + 32.0 * s, 282.0 * s, WHITE, &shown);
    if !busy && focus.0 != usize::MAX {
        panel(canvas, left + 36.0 * s + end, 246.0 * s, 3.0 * s, 44.0 * s, 0.0, ACCENT);
    }
    let (line, colour) = if busy { ("Looking the address up\u{2026}", ACCENT) } else { (error, WARN) };
    draw_text(canvas, &fonts.regular, 26.0 * s, left, 356.0 * s, colour, &fit(&fonts.regular, 26.0 * s, line, width));

    let (key_width, key_height, gap) = (150.0 * s, 84.0 * s, 12.0 * s);
    let keys_width = KEYS[0].len() as f32 * (key_width + gap) - gap;
    let (x0, y0) = ((canvas.width as f32 - keys_width) / 2.0, 400.0 * s);
    for (row, keys) in KEYS.iter().enumerate() {
        for (col, label) in keys.iter().enumerate() {
            let (x, y) = (x0 + col as f32 * (key_width + gap), y0 + row as f32 * (key_height + gap));
            let on = focus == (row, col) && !busy;
            let action = matches!(*label, "Del" | "OK");
            panel(canvas, x, y, key_width, key_height, 16.0 * s, if on { WHITE } else if action { rgba(0x7c9cff, 70) } else { rgba(0xffffff, 26) });
            let font = if label.len() > 1 { &fonts.regular } else { &fonts.medium };
            centred(canvas, font, if label.len() > 1 { 28.0 } else { 34.0 } * s, x + key_width / 2.0, y + 54.0 * s, if on { DARK } else { WHITE }, label);
        }
    }
    hint(canvas, fonts, s, "You can also type on your phone's remote  \u{00b7}  OK looks the address up  \u{00b7}  Back cancels");
}

/// A panel in the middle of the dimmed screen: a title, some lines and a row of buttons.
fn dialog(canvas: &mut Canvas, fonts: &Fonts, s: f32, title: &str, lines: &[(String, Color)], labels: &[&str], focused: usize) {
    dim(canvas);
    let width = 1120.0 * s;
    let text_width = width - 112.0 * s;
    let wrapped: Vec<(String, Color)> = lines.iter().flat_map(|(text, colour)| wrap(&fonts.regular, 26.0 * s, text, text_width, 3).into_iter().map(|l| (l, *colour))).collect();
    let height = (140.0 + wrapped.len() as f32 * 40.0 + 130.0) * s;
    let (x, y) = ((canvas.width as f32 - width) / 2.0, (canvas.height as f32 - height) / 2.0);
    panel(canvas, x, y, width, height, 32.0 * s, rgba(0x1c2233, 255));
    draw_text(canvas, &fonts.medium, 40.0 * s, x + 56.0 * s, y + 84.0 * s, WHITE, &fit(&fonts.medium, 40.0 * s, title, text_width));
    for (index, (text, colour)) in wrapped.iter().enumerate() {
        draw_text(canvas, &fonts.regular, 26.0 * s, x + 56.0 * s, y + (150.0 + index as f32 * 40.0) * s, *colour, text);
    }
    buttons(canvas, fonts, s, x + 56.0 * s, y + height - 112.0 * s, labels, Some(focused));
}

/// The smallest rectangle (x, y, width, height) around the pixels that differ, if any do.
pub fn changed(old: &[u32], new: &[u32], width: usize) -> Option<(usize, usize, usize, usize)> {
    let differs = |row: usize| old[row * width..(row + 1) * width] != new[row * width..(row + 1) * width];
    let rows = new.len() / width;
    let top = (0..rows).find(|row| differs(*row))?;
    let bottom = (top..rows).rev().find(|row| differs(*row)).unwrap_or(top);
    let (mut from, mut to) = (width, 0);
    for row in top..=bottom {
        let (a, b) = (&old[row * width..(row + 1) * width], &new[row * width..(row + 1) * width]);
        if let Some(first) = (0..from.min(width)).find(|x| a[*x] != b[*x]) {
            from = first;
        }
        if let Some(last) = (to..width).rev().find(|x| a[*x] != b[*x]) {
            to = last + 1;
        }
    }
    Some((from, top, to.saturating_sub(from).max(1), bottom - top + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_changed_part_is_found() {
        let width = 8;
        let old = vec![0u32; width * 6];
        let mut new = old.clone();
        assert_eq!(changed(&old, &new, width), None);
        new[2 * width + 3] = 1;
        assert_eq!(changed(&old, &new, width), Some((3, 2, 1, 1)));
        new[4 * width + 6] = 1;
        new[3 * width + 1] = 1;
        assert_eq!(changed(&old, &new, width), Some((1, 2, 6, 3)));
    }

    #[test]
    fn panels_stay_inside_and_fill_their_middle() {
        let mut canvas = Canvas::new(40, 30);
        panel(&mut canvas, 5.0, 5.0, 20.0, 10.0, 4.0, rgba(0x112233, 255));
        assert_eq!(canvas.pixels[10 * 40 + 15], 0xff112233);
        assert_eq!(canvas.pixels[5 * 40 + 5], 0, "the corner is cut");
        assert_eq!(canvas.pixels[4 * 40 + 15], 0);
        assert_eq!(canvas.pixels[10 * 40 + 26], 0);
        // Off the canvas: nothing is written out of bounds.
        panel(&mut canvas, -10.0, -10.0, 100.0, 100.0, 8.0, rgba(0xffffff, 128));
        panel(&mut canvas, 35.0, 25.0, 50.0, 50.0, 0.0, rgba(0xffffff, 255));
        assert_eq!(canvas.pixels[29 * 40 + 39], 0xffffffff);
    }
}
