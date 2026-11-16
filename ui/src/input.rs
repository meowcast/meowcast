//! Development input: a FIFO (default /run/ui-keys). From the serial shell:
//!   printf ddd > /run/ui-keys      # right x3     (WASD, vi hjkl or ANSI arrows)
//! e/Enter = select, b/x = back, m/g = Home (back to the launcher from an app), p = screenshot, q = quit. Opened read-write so writers closing never
//! produces EOF. A USB keyboard / IR remote (evdev) can feed the same Key events later.
//! The digits 1-6 are the hardware button's steps, written by meow-buttond
//! (os/buttond.c): they are not keys, and the phone remote cannot send them.

use crate::home::Key;
use std::ffi::CString;
use std::os::fd::RawFd;

pub enum Input {
    Key(Key),
    Quit,
    Shot,
    /// A step of the hardware button (meow-buttond).
    Button(Button),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// Pressed briefly: show (or hide) the pairing screen.
    Short,
    /// Held for 3 s: the factory-reset warning starts.
    Warn,
    /// Held for 10 s: letting go now erases.
    Armed,
    /// Let go during the warning.
    Cancelled,
    /// Held past the window.
    TooLong,
    /// Let go inside the window: the daemon is erasing.
    Erasing,
}

pub struct Keys {
    pub fd: RawFd,
    esc: Vec<u8>,
}

impl Keys {
    pub fn open(path: &str) -> Option<Keys> {
        let c = CString::new(path).ok()?;
        unsafe {
            libc::mkfifo(c.as_ptr(), 0o600);
            let fd = libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC);
            (fd >= 0).then_some(Keys { fd, esc: Vec::new() })
        }
    }

    pub fn read(&mut self, out: &mut Vec<Input>) {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                return;
            }
            for &b in &buf[..n as usize] {
                if !self.esc.is_empty() || b == 0x1b {
                    self.esc.push(b);
                    if self.esc.len() == 3 {
                        let k = match self.esc[2] {
                            b'A' => Some(Key::Up),
                            b'B' => Some(Key::Down),
                            b'C' => Some(Key::Right),
                            b'D' => Some(Key::Left),
                            _ => None,
                        };
                        out.extend(k.map(Input::Key));
                        self.esc.clear();
                    }
                    continue;
                }
                let k = match b {
                    b'a' | b'h' => Input::Key(Key::Left),
                    b'd' | b'l' => Input::Key(Key::Right),
                    b'w' | b'k' => Input::Key(Key::Up),
                    b's' | b'j' => Input::Key(Key::Down),
                    b'e' | b'\n' | b'\r' => Input::Key(Key::Enter),
                    b'b' | b'x' => Input::Key(Key::Back),
                    b'm' | b'g' => Input::Key(Key::Home),
                    b'q' => Input::Quit,
                    b'p' => Input::Shot,
                    b'1' => Input::Button(Button::Short),
                    b'2' => Input::Button(Button::Warn),
                    b'3' => Input::Button(Button::Armed),
                    b'4' => Input::Button(Button::Cancelled),
                    b'5' => Input::Button(Button::TooLong),
                    b'6' => Input::Button(Button::Erasing),
                    _ => continue,
                };
                out.push(k);
            }
        }
    }
}
