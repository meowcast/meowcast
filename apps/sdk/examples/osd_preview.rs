//! Draw the volume pop-up to PPM files, to look at it on a desk:
//!   cargo run -p meow-app --features osd --example osd_preview -- FONT_DIR OUT_DIR [SCREEN_HEIGHT]
//! Each state is shown over a dark and a bright background (as over the launcher and a video).
use meow_app::osd;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(fonts), Some(out)) = (args.first(), args.get(1)) else {
        eprintln!("usage: osd_preview FONT_DIR OUT_DIR [SCREEN_HEIGHT]");
        std::process::exit(2);
    };
    let screen_h: u32 = args.get(2).and_then(|h| h.parse().ok()).unwrap_or(1080);
    let mut renderer = osd::Renderer::from_font_dir(fonts, screen_h).expect("font");
    std::fs::create_dir_all(out).expect("output directory");
    let states = [(100, false), (70, false), (45, false), (20, false), (0, false), (60, true)];
    let (w, h) = osd::sprite_size(screen_h);
    let (cols, rows) = (2usize, states.len());
    let (sheet_w, sheet_h) = (w as usize * cols, h as usize * rows);
    let mut sheet = vec![0u32; sheet_w * sheet_h];
    for (i, px) in sheet.iter_mut().enumerate() {
        // Left: the launcher's dark blue; right: a bright picture.
        let (x, y) = (i % sheet_w, i / sheet_w);
        *px = if x < w as usize { 0xff14_1a2c } else { 0xff00_0000 | ((150 + (y * 60 / sheet_h)) as u32) << 16 | ((190 - (x * 40 / sheet_w)) as u32) << 8 | 0xe0 };
    }
    let started = std::time::Instant::now();
    for (row, (level, muted)) in states.iter().enumerate() {
        let sprite = renderer.render(*level, *muted).clone();
        for col in 0..cols {
            sprite.blend_into(&mut sheet, sheet_w, (col * w as usize) as i32, (row * h as usize) as i32, 1.0);
        }
    }
    let per = started.elapsed() / states.len() as u32;
    let mut ppm = format!("P6\n{sheet_w} {sheet_h}\n255\n").into_bytes();
    ppm.extend(sheet.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]));
    let path = format!("{out}/volume-osd-{screen_h}.ppm");
    std::fs::write(&path, ppm).expect("write");
    println!("{path}: {} states, {} us per state (render + two blends)", states.len(), per.as_micros());
}
