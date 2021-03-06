//! module to handle keystrokes
//!
//! ```no_run
//! use tuikit::input::KeyBoard;
//! use tuikit::key::Key;
//! use std::time::Duration;
//! let mut keyboard = KeyBoard::new_with_tty();
//! let key = keyboard.next_key();
//! ```

use crate::key::Key::*;
use crate::key::{Key, MouseButton};
use crate::raw::get_tty;
use crate::spinlock::SpinLock;
use crate::sys::file::wait_until_ready;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use std::collections::VecDeque;
use std::error::Error;
use std::fs::File;
use std::io::prelude::*;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::sync::Arc;
use std::time::Duration;

pub trait ReadAndAsRawFd: Read + AsRawFd + Send {}

const KEY_WAIT: Duration = Duration::from_millis(10);

impl<T> ReadAndAsRawFd for T where T: Read + AsRawFd + Send {}

pub struct KeyBoard {
    file: Box<dyn ReadAndAsRawFd>,
    sig_tx: Arc<SpinLock<File>>,
    sig_rx: File,
    buf: VecDeque<char>,
}

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

// https://www.xfree86.org/4.8.0/ctlseqs.html
impl KeyBoard {
    pub fn new(file: Box<dyn ReadAndAsRawFd>) -> Self {
        // the self-pipe trick for interrupt `select`
        let (rx, tx) = nix::unistd::pipe().expect("failed to set pipe");

        // set the signal pipe to non-blocking mode
        let flag = fcntl(rx, FcntlArg::F_GETFL).expect("Get fcntl failed");
        let mut flag = OFlag::from_bits_truncate(flag);
        flag.insert(OFlag::O_NONBLOCK);
        let _ = fcntl(rx, FcntlArg::F_SETFL(flag));

        // set file to non-blocking mode
        let flag = fcntl(file.as_raw_fd(), FcntlArg::F_GETFL).expect("Get fcntl failed");
        let mut flag = OFlag::from_bits_truncate(flag);
        flag.insert(OFlag::O_NONBLOCK);
        let _ = fcntl(file.as_raw_fd(), FcntlArg::F_SETFL(flag));

        KeyBoard {
            file,
            sig_tx: Arc::new(SpinLock::new(unsafe { File::from_raw_fd(tx) })),
            sig_rx: unsafe { File::from_raw_fd(rx) },
            buf: VecDeque::new(),
        }
    }

    pub fn new_with_tty() -> Self {
        Self::new(Box::new(
            get_tty().expect("KeyBoard::new_with_tty: failed to get tty"),
        ))
    }

    pub fn get_interrupt_handler(&self) -> KeyboardHandler {
        KeyboardHandler {
            handler: self.sig_tx.clone(),
        }
    }

    fn get_chars(&mut self, timeout: Duration) -> Result<()> {
        let mut reader_buf = [0; 1];

        // clear interrupt signal
        while let Ok(_) = self.sig_rx.read(&mut reader_buf) {}

        wait_until_ready(
            self.file.as_raw_fd(),
            Some(self.sig_rx.as_raw_fd()),
            timeout,
        )?; // wait timeout

        let mut buf = Vec::with_capacity(10);
        while let Ok(_) = self.file.read(&mut reader_buf) {
            buf.push(reader_buf[0]);
        }

        let chars = String::from_utf8(buf).expect("Non UTF8 in input");
        for ch in chars.chars() {
            self.buf.push_back(ch);
        }
        Ok(())
    }

    fn next_char(&mut self) -> Result<char> {
        self.next_char_timeout(Duration::new(0, 0))
    }

    fn next_char_timeout(&mut self, timeout: Duration) -> Result<char> {
        if self.buf.is_empty() {
            self.get_chars(timeout)?;
        }
        self.buf
            .pop_front()
            .ok_or("no more bytes in the buffer".into())
    }

    /// Wait next key stroke
    pub fn next_key(&mut self) -> Result<Key> {
        self.next_key_timeout(Duration::new(0, 0))
    }

    /// Wait `timeout` until next key stroke
    pub fn next_key_timeout(&mut self, timeout: Duration) -> Result<Key> {
        let ch = self.next_char_timeout(timeout)?;
        match ch {
            '\u{00}' => Ok(Ctrl(' ')),
            '\u{01}' => Ok(Ctrl('a')),
            '\u{02}' => Ok(Ctrl('b')),
            '\u{03}' => Ok(Ctrl('c')),
            '\u{04}' => Ok(Ctrl('d')),
            '\u{05}' => Ok(Ctrl('e')),
            '\u{06}' => Ok(Ctrl('f')),
            '\u{07}' => Ok(Ctrl('g')),
            '\u{08}' => Ok(Ctrl('h')),
            '\u{09}' => Ok(Tab),
            '\u{0A}' => Ok(Ctrl('j')),
            '\u{0B}' => Ok(Ctrl('k')),
            '\u{0C}' => Ok(Ctrl('l')),
            '\u{0D}' => Ok(Enter),
            '\u{0E}' => Ok(Ctrl('n')),
            '\u{0F}' => Ok(Ctrl('o')),
            '\u{10}' => Ok(Ctrl('p')),
            '\u{11}' => Ok(Ctrl('q')),
            '\u{12}' => Ok(Ctrl('r')),
            '\u{13}' => Ok(Ctrl('s')),
            '\u{14}' => Ok(Ctrl('t')),
            '\u{15}' => Ok(Ctrl('u')),
            '\u{16}' => Ok(Ctrl('v')),
            '\u{17}' => Ok(Ctrl('w')),
            '\u{18}' => Ok(Ctrl('x')),
            '\u{19}' => Ok(Ctrl('y')),
            '\u{1A}' => Ok(Ctrl('z')),
            '\u{1B}' => self.escape_sequence(),
            '\u{7F}' => Ok(Backspace),
            ch => Ok(Char(ch)),
        }
    }

    fn escape_sequence(&mut self) -> Result<Key> {
        let seq1 = self.next_char_timeout(KEY_WAIT).unwrap_or('\u{1B}');
        match seq1 {
            '[' => self.escape_csi(),
            'O' => self.escape_o(),
            _ => self.parse_alt(seq1),
        }
    }

    fn parse_alt(&mut self, ch: char) -> Result<Key> {
        match ch {
            '\u{1B}' => {
                match self.next_char_timeout(KEY_WAIT) {
                    Ok('[') => {}
                    Ok(c) => {
                        return Err(format!("unsupported esc sequence: ESC ESC {:?}", c).into());
                    }
                    Err(_) => return Ok(ESC),
                }

                match self.escape_csi() {
                    Ok(Up) => Ok(AltUp),
                    Ok(Down) => Ok(AltDown),
                    Ok(Left) => Ok(AltLeft),
                    Ok(Right) => Ok(AltRight),
                    Ok(PageUp) => Ok(AltPageUp),
                    Ok(PageDown) => Ok(AltPageDown),
                    _ => Err(format!("unsupported esc sequence: ESC ESC [ ...").into()),
                }
            }
            '\u{00}' => Ok(CtrlAlt(' ')),
            '\u{01}' => Ok(CtrlAlt('a')),
            '\u{02}' => Ok(CtrlAlt('b')),
            '\u{03}' => Ok(CtrlAlt('c')),
            '\u{04}' => Ok(CtrlAlt('d')),
            '\u{05}' => Ok(CtrlAlt('e')),
            '\u{06}' => Ok(CtrlAlt('f')),
            '\u{07}' => Ok(CtrlAlt('g')),
            '\u{08}' => Ok(CtrlAlt('h')),
            '\u{09}' => Ok(AltTab),
            '\u{0A}' => Ok(CtrlAlt('j')),
            '\u{0B}' => Ok(CtrlAlt('k')),
            '\u{0C}' => Ok(CtrlAlt('l')),
            '\u{0D}' => Ok(AltEnter),
            '\u{0E}' => Ok(CtrlAlt('n')),
            '\u{0F}' => Ok(CtrlAlt('o')),
            '\u{10}' => Ok(CtrlAlt('p')),
            '\u{11}' => Ok(CtrlAlt('q')),
            '\u{12}' => Ok(CtrlAlt('r')),
            '\u{13}' => Ok(CtrlAlt('s')),
            '\u{14}' => Ok(CtrlAlt('t')),
            '\u{15}' => Ok(CtrlAlt('u')),
            '\u{16}' => Ok(CtrlAlt('v')),
            '\u{17}' => Ok(CtrlAlt('w')),
            '\u{18}' => Ok(CtrlAlt('x')),
            '\u{19}' => Ok(AltBackTab),
            '\u{1A}' => Ok(CtrlAlt('z')),
            '\u{7F}' => Ok(AltBackspace),
            ch => Ok(Alt(ch)),
        }
    }

    fn escape_csi(&mut self) -> Result<Key> {
        let cursor_pos = self.parse_cursor_report();
        if cursor_pos.is_ok() {
            return cursor_pos;
        }

        let seq2 = self.next_char()?;
        match seq2 {
            '0' | '9' => Err(format!("unsupported esc sequence: ESC [ {:?}", seq2).into()),
            '1'...'8' => self.extended_escape(seq2),
            '[' => {
                // Linux Console ESC [ [ _
                let seq3 = self.next_char()?;
                match seq3 {
                    'A' => Ok(F(1)),
                    'B' => Ok(F(2)),
                    'C' => Ok(F(3)),
                    'D' => Ok(F(4)),
                    'E' => Ok(F(5)),
                    _ => Err(format!("unsupported esc sequence: ESC [ [ {:?}", seq3).into()),
                }
            }
            'A' => Ok(Up),    // kcuu1
            'B' => Ok(Down),  // kcud1
            'C' => Ok(Right), // kcuf1
            'D' => Ok(Left),  // kcub1
            'H' => Ok(Home),  // khome
            'F' => Ok(End),
            'Z' => Ok(BackTab),
            'M' => {
                // X10 emulation mouse encoding: ESC [ M Bxy (6 characters only)
                let cb = self.next_char()? as u8;
                // (1, 1) are the coords for upper left.
                let cx = (self.next_char()? as u8).saturating_sub(32) as u16;
                let cy = (self.next_char()? as u8).saturating_sub(32) as u16;
                match cb & 0b11 {
                    0 => {
                        if cb & 0x40 != 0 {
                            Ok(MousePress(MouseButton::WheelUp, cx, cy))
                        } else {
                            Ok(MousePress(MouseButton::Left, cx, cy))
                        }
                    }
                    1 => {
                        if cb & 0x40 != 0 {
                            Ok(MousePress(MouseButton::WheelDown, cx, cy))
                        } else {
                            Ok(MousePress(MouseButton::Middle, cx, cy))
                        }
                    }
                    2 => Ok(MousePress(MouseButton::Right, cx, cy)),
                    3 => Ok(MouseRelease(cx, cy)),
                    _ => Err(
                        format!("unsupported esc sequence: ESC M {:?}{:?}{:?}", cb, cx, cy).into(),
                    ),
                }
            }
            '<' => {
                // xterm mouse encoding:
                // ESC [ < Cb ; Cx ; Cy ; (M or m)
                if !self.buf.contains(&'m') && !self.buf.contains(&'M') {
                    return Err(
                        format!("unknown esc sequence ESC [ < (not ending with m/M)").into(),
                    );
                }

                let mut str_buf = String::new();
                let mut c = self.next_char()?;
                while c != 'm' && c != 'M' {
                    str_buf.push(c);
                    c = self.next_char()?;
                }
                let nums = &mut str_buf.split(';');

                let cb = nums.next().unwrap().parse::<u16>().unwrap();
                let cx = nums.next().unwrap().parse::<u16>().unwrap();
                let cy = nums.next().unwrap().parse::<u16>().unwrap();

                match cb {
                    0...2 | 64...65 => {
                        let button = match cb {
                            0 => MouseButton::Left,
                            1 => MouseButton::Middle,
                            2 => MouseButton::Right,
                            64 => MouseButton::WheelUp,
                            65 => MouseButton::WheelDown,
                            _ => {
                                return Err(
                                    format!("unknown sequence: ESC [ < {} {}", str_buf, c).into()
                                );
                            }
                        };

                        match c {
                            'M' => Ok(MousePress(button, cx, cy)),
                            'm' => Ok(MouseRelease(cx, cy)),
                            _ => Err(format!("unknown sequence: ESC [ < {} {}", str_buf, c).into()),
                        }
                    }
                    32 => Ok(MouseHold(cx, cy)),
                    _ => Err(format!("unknown sequence: ESC [ < {} {}", str_buf, c).into()),
                }
            }
            _ => Err(format!("unsupported esc sequence: ESC [ {:?}", seq2).into()),
        }
    }

    fn parse_cursor_report(&mut self) -> Result<Key> {
        if self.buf.contains(&';') && self.buf.contains(&'R') {
            let mut row = String::new();
            let mut col = String::new();

            while self.buf.front() != Some(&';') {
                row.push(self.buf.pop_front().unwrap());
            }
            self.buf.pop_front();

            while self.buf.front() != Some(&'R') {
                col.push(self.buf.pop_front().unwrap());
            }
            self.buf.pop_front();

            let row_num = row.parse::<u16>()?;
            let col_num = col.parse::<u16>()?;
            Ok(CursorPos(row_num - 1, col_num - 1))
        } else {
            Err(format!("buffer did not contain cursor position response").into())
        }
    }

    fn extended_escape(&mut self, seq2: char) -> Result<Key> {
        let seq3 = self.next_char()?;
        if seq3 == '~' {
            match seq2 {
                '1' | '7' => Ok(Home), // tmux, xrvt
                '2' => Ok(Insert),
                '3' => Ok(Delete),    // kdch1
                '4' | '8' => Ok(End), // tmux, xrvt
                '5' => Ok(PageUp),    // kpp
                '6' => Ok(PageDown),  // knp
                _ => Err(format!("unsupported esc sequence: ESC [ {} ~", seq2).into()),
            }
        } else if seq3.is_digit(10) {
            let mut str_buf = String::new();
            str_buf.push(seq2);
            str_buf.push(seq3);

            let mut seq_last = self.next_char()?;
            while seq_last != 'M' && seq_last != '~' {
                str_buf.push(seq_last);
                seq_last = self.next_char()?;
            }

            match seq_last {
                'M' => {
                    // rxvt mouse encoding:
                    // ESC [ Cb ; Cx ; Cy ; M
                    let mut nums = str_buf.split(';');

                    let cb = nums.next().unwrap().parse::<u16>().unwrap();
                    let cx = nums.next().unwrap().parse::<u16>().unwrap();
                    let cy = nums.next().unwrap().parse::<u16>().unwrap();

                    match cb {
                        32 => Ok(MousePress(MouseButton::Left, cx, cy)),
                        33 => Ok(MousePress(MouseButton::Middle, cx, cy)),
                        34 => Ok(MousePress(MouseButton::Right, cx, cy)),
                        35 => Ok(MouseRelease(cx, cy)),
                        64 => Ok(MouseHold(cx, cy)),
                        96 | 97 => Ok(MousePress(MouseButton::WheelUp, cx, cy)),
                        _ => Err(format!("unsupported esc sequence: ESC [ {} M", str_buf).into()),
                    }
                }
                '~' => {
                    let num: u8 = str_buf.parse().unwrap();
                    match num {
                        v @ 11...15 => Ok(F(v - 10)),
                        v @ 17...21 => Ok(F(v - 11)),
                        v @ 23...24 => Ok(F(v - 12)),
                        _ => Err(format!("unsupported esc sequence: ESC [ {} ~", str_buf).into()),
                    }
                }
                _ => unreachable!(),
            }
        } else if seq3 == ';' {
            let seq4 = self.next_char()?;
            if seq4.is_digit(10) {
                let seq5 = self.next_char()?;
                if seq2 == '1' {
                    match (seq4, seq5) {
                        ('5', 'A') => Ok(CtrlUp),
                        ('5', 'B') => Ok(CtrlDown),
                        ('5', 'C') => Ok(CtrlRight),
                        ('5', 'D') => Ok(CtrlLeft),
                        ('4', 'A') => Ok(AltShiftUp),
                        ('4', 'B') => Ok(AltShiftDown),
                        ('4', 'C') => Ok(AltShiftRight),
                        ('4', 'D') => Ok(AltShiftLeft),
                        ('3', 'H') => Ok(AltHome),
                        ('3', 'F') => Ok(AltEnd),
                        ('2', 'A') => Ok(ShiftUp),
                        ('2', 'B') => Ok(ShiftDown),
                        ('2', 'C') => Ok(ShiftRight),
                        ('2', 'D') => Ok(ShiftLeft),
                        _ => Err(format!(
                            "unsupported esc sequence: ESC [ 1 ; {} {:?}",
                            seq4, seq5
                        )
                        .into()),
                    }
                } else {
                    Err(format!(
                        "unsupported esc sequence: ESC [ {} ; {} {:?}",
                        seq2, seq4, seq5
                    )
                    .into())
                }
            } else {
                Err(format!("unsupported esc sequence: ESC [ {} ; {:?}", seq2, seq4).into())
            }
        } else {
            match (seq2, seq3) {
                ('5', 'A') => Ok(CtrlUp),
                ('5', 'B') => Ok(CtrlDown),
                ('5', 'C') => Ok(CtrlRight),
                ('5', 'D') => Ok(CtrlLeft),
                _ => Err(format!("unsupported esc sequence: ESC [ {} {:?}", seq2, seq3).into()),
            }
        }
    }

    // SSS3
    fn escape_o(&mut self) -> Result<Key> {
        let seq2 = self.next_char()?;
        match seq2 {
            'A' => Ok(Up),    // kcuu1
            'B' => Ok(Down),  // kcud1
            'C' => Ok(Right), // kcuf1
            'D' => Ok(Left),  // kcub1
            'F' => Ok(End),   // kend
            'H' => Ok(Home),  // khome
            'P' => Ok(F(1)),  // kf1
            'Q' => Ok(F(2)),  // kf2
            'R' => Ok(F(3)),  // kf3
            'S' => Ok(F(4)),  // kf4
            'a' => Ok(CtrlUp),
            'b' => Ok(CtrlDown),
            'c' => Ok(CtrlRight), // rxvt
            'd' => Ok(CtrlLeft),  // rxvt
            _ => Err(format!("unsupported esc sequence: ESC O {:?}", seq2).into()),
        }
    }
}

pub struct KeyboardHandler {
    handler: Arc<SpinLock<File>>,
}

impl KeyboardHandler {
    pub fn interrupt(&self) {
        let mut handler = self.handler.lock();
        let _ = handler.write_all(b"x\n");
    }
}
