use std::fmt;

#[derive(Clone, Debug)]
pub struct Printbuf {
    pub buf: String,
    pub tabstop: u32,
    pub indent: u32,
    pub suppress: bool,
    pub atomic: bool,
}

impl Printbuf {
    pub fn new() -> Self {
        Printbuf {
            buf: String::new(),
            tabstop: 8,
            indent: 0,
            suppress: false,
            atomic: false,
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }

    pub fn exit(&mut self) {
        self.buf.clear();
    }

    pub fn indent_add(&mut self, inc: u32) {
        self.indent += inc;
    }

    pub fn indent_sub(&mut self, inc: u32) {
        self.indent = self.indent.saturating_sub(inc);
    }

    pub fn tabstop_reset(&mut self) {
        self.tabstop = 8;
    }

    pub fn tabstop_set(&mut self, stops: &[u32]) {
        if let Some(&s) = stops.first() {
            self.tabstop = s;
        }
    }

    pub fn as_raw(&mut self) -> *mut Printbuf {
        self as *mut Printbuf
    }

    pub fn indent(&mut self, n: u32) -> PrintbufIndent {
        self.indent_add(n);
        PrintbufIndent { pb: self, n }
    }

    pub fn newline(&mut self) {
        self.buf.push('\n');
        for _ in 0..self.indent {
            self.buf.push(' ');
        }
    }

    pub fn string(&self) -> &str {
        &self.buf
    }
}

impl fmt::Write for Printbuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.buf.push_str(s);
        Ok(())
    }
}

pub struct PrintbufIndent<'a> {
    pb: &'a mut Printbuf,
    n: u32,
}

impl<'a> Drop for PrintbufIndent<'a> {
    fn drop(&mut self) {
        self.pb.indent_sub(self.n);
    }
}

impl<'a> fmt::Write for PrintbufIndent<'a> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.pb.write_str(s)
    }
}

pub fn prt_units(pb: &mut Printbuf, v: u64, units: u32) {
    let prefixes = ["", "K", "M", "G", "T", "P"];
    let mut idx = 0;
    let mut val = v as f64;
    while val >= 1024.0 && idx < 5 {
        val /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        let _ = write!(pb, "{}", v);
    } else {
        let _ = write!(pb, "{:.1}{}", val, prefixes[idx]);
    }
}

pub fn prt_u64(pb: &mut Printbuf, v: u64) {
    let _ = write!(pb, "{}", v);
}

pub fn prt_i64(pb: &mut Printbuf, v: i64) {
    let _ = write!(pb, "{}", v);
}

pub fn prt_string(pb: &mut Printbuf, s: &str) {
    pb.buf.push_str(s);
}

pub fn prt_printf(pb: &mut Printbuf, fmt_str: &str, args: std::fmt::Arguments) {
    let _ = write!(pb, "{}", fmt_str);
}

pub fn printbuf_to_formatter(pb: &Printbuf, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{}", pb.buf)
}
