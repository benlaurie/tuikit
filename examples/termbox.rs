use tuikit::termbox;
use std::thread;
use std::sync::{Arc, Mutex};
use std::borrow::BorrowMut;
use std::time::Duration;
use tuikit::key::Key;
use tuikit::event::Event;

fn main() {
    let th = thread::spawn(move || {
        let term = termbox::hold();
        while let Ok(ev) = term.poll_event() {
            if let Event::Key(Key::Char('q')) = ev {
                break;
            }
            println!("{:?}", ev)
        }
    });
    th.join();
}