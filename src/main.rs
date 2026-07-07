// RPNx — a terminal RPN / XRPN scientific calculator.
//
// The stack model, command set, and number formatting all live in the shared
// engine crate `rpnx_core` (also the brain of the RPNx phone app), so this
// binary is a thin crust TUI: it maps keys to engine calls, then repaints the
// stack + the function legend. HP-41CX / desktop-XRPN semantics throughout.
//
// scribe hook: run `rpnx --emit-x`. On a normal quit (Q / ESC) the X register
// is printed to stdout so the caller can paste it back; a cancel-quit (Ctrl+C)
// prints nothing. Standalone (no flag) never prints.
//
// Design goals (Fe2O3): cold when idle (blocking key read, no timers), fast
// startup, one engine step per keystroke. State persists to ~/.config/rpnx/state.

use crust::{Crust, Input, Pane, style};
use rpnx_core::{
    display, execute, key_backspace, key_chs, key_digit, key_dot, key_eex, key_enter, new_state,
    parse_state, serialize_state, CalcState,
};
use std::path::PathBuf;

// Palette (xterm-256), aligned with the rest of the suite.
const C_TITLE: u8 = 214; // RPNx accent (orange)
const C_KEY: u8 = 214; // legend trigger key (orange)
const C_DESC: u8 = 245; // legend description (dim)
const C_LABEL: u8 = 245; // stack register label
const C_VAL: u8 = 250; // stack register value
const C_XVAL: u8 = 255; // X register value (bright)
const C_MODE: u8 = 108; // mode line (muted green)
const C_MSG: u8 = 214; // transient message
const C_BAR_BG: u8 = 236; // title / foot background
const C_BOX: u8 = 240; // stack box border
const C_GRP: u8 = 109; // legend group label

/// A cycling SHIFT page. The SHIFT key (TAB) steps base -> f -> g -> h -> base.
/// While a page is active its functions overlay the listed keys (recoloured to
/// the page colour); every other base key still works. Mirrors the phone app's
/// coloured f/g/h shift pages.
struct ShiftPage {
    tag: &'static str,
    color: u8,
    keys: &'static [(&'static str, &'static str, &'static str)], // (key, label, cmd)
}
const SHIFT_PAGES: &[ShiftPage] = &[
    ShiftPage {
        tag: "f",
        color: 222, // gold — powers
        keys: &[
            ("s", "x\u{00b2}", "sqr"),
            ("c", "x\u{00b3}", "cube"),
            ("e", "e\u{02e3}", "exp"),
            ("t", "10\u{02e3}", "tenx"),
            ("y", "\u{02e3}\u{221a}y", "root"),
        ],
    },
    ShiftPage {
        tag: "g",
        color: 75, // blue — stats & parts
        keys: &[
            ("+", "\u{03a3}+", "splus"),
            ("-", "\u{03a3}\u{2212}", "sminus"),
            ("m", "x\u{0304}", "mean"),
            ("d", "sd", "sdev"),
            ("z", "CL\u{03a3}", "cls"),
            ("i", "INT", "int"),
            ("f", "FRC", "frc"),
            ("x", "drop", "drop"),
            ("%", "\u{0394}%", "percentch"),
        ],
    },
    ShiftPage {
        tag: "h",
        color: 78, // green — modes & convert
        keys: &[
            ("e", "ENG", "eng"),
            ("g", "GRAD", "grad"),
            ("N", "RND", "rnd"),
            ("h", "HMS", "hms"),
            ("H", "HR", "hr"),
            ("p", "\u{2192}P", "r_p"),
            ("P", "\u{2192}R", "p_r"),
        ],
    },
];

fn state_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/rpnx/state")
}

fn load_state() -> CalcState {
    match std::fs::read_to_string(state_path()) {
        Ok(json) if !json.trim().is_empty() => parse_state(json),
        _ => new_state(),
    }
}

fn save_state(s: &CalcState) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serialize_state(s.clone()));
}

struct App {
    cols: u16,
    top: Pane,
    main: Pane,
    foot: Pane,
    state: CalcState,
    undo: Vec<CalcState>,
    msg: String,
    page: usize, // 0 = base; 1..=3 index SHIFT_PAGES
}

impl App {
    fn new() -> Self {
        let (cols, rows) = Crust::terminal_size();
        let mut top = Pane::new(1, 1, cols, 1, C_TITLE as u16, C_BAR_BG as u16);
        top.wrap = false;
        top.scroll = false;
        let body_h = rows.saturating_sub(3);
        let mut main = Pane::new(1, 3, cols, body_h, C_VAL as u16, 0);
        main.wrap = false;
        main.scroll = false;
        let mut foot = Pane::new(1, rows, cols, 1, C_DESC as u16, C_BAR_BG as u16);
        foot.wrap = false;
        foot.scroll = false;
        App {
            cols,
            top,
            main,
            foot,
            state: load_state(),
            undo: Vec::new(),
            msg: String::new(),
            page: 0,
        }
    }

    /// The command bound to `key` on the active shift page, if any.
    fn page_cmd(&self, key: &str) -> Option<&'static str> {
        if self.page == 0 {
            return None;
        }
        SHIFT_PAGES[self.page - 1]
            .keys
            .iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, _, c)| *c)
    }

    /// Snapshot state before a mutating action so `u` can undo it.
    fn push_undo(&mut self) {
        self.undo.push(self.state.clone());
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
    }

    /// Run a named engine command, recording undo and surfacing any error.
    fn run_cmd(&mut self, cmd: &str) {
        self.push_undo();
        let r = execute(self.state.clone(), cmd.to_string());
        self.state = r.state;
        self.msg = r.error.unwrap_or_default();
    }

    /// Prompt for a value in the foot line (returns trimmed input; "" on cancel).
    fn ask(&mut self, prompt: &str) -> String {
        let s = self.foot.ask(prompt, "");
        s.trim().to_string()
    }

    // ---- rendering -------------------------------------------------------

    fn render_top(&mut self) {
        let d = display(self.state.clone());
        let base = format!(" RPNx    {}", d.mode);
        // Active shift-page tag, in the page colour.
        let (tag_plain, tag_styled) = if self.page != 0 {
            let p = &SHIFT_PAGES[self.page - 1];
            let t = format!("  SH {}", p.tag);
            (t.clone(), style::bold(&style::fg(&t, p.color)))
        } else {
            (String::new(), String::new())
        };
        let hint = "TAB shift \u{00b7} H help \u{00b7} Q quit ";
        let pad = (self.cols as usize).saturating_sub(
            crust::display_width(&base) + crust::display_width(&tag_plain) + crust::display_width(hint),
        );
        self.top.say(&format!(
            "{}{}{}{}",
            style::bold(&style::fg(&base, C_TITLE)),
            tag_styled,
            " ".repeat(pad),
            style::fg(hint, C_DESC)
        ));
    }

    fn render_main(&mut self) {
        let d = display(self.state.clone());
        // Right-align the register values in a shared field.
        let vals = [&d.last_x, &d.t, &d.z, &d.y, &d.x];
        let field = vals
            .iter()
            .map(|v| crust::display_width(v))
            .max()
            .unwrap_or(0)
            .max(12);
        // Indent so the register labels (L/T/…/X) sit under the first legend
        // column (the "0-9" cell, which starts 10 cols in: "  " + 5-wide group
        // label + "   "). Box border at col 8, label at col 10.
        let indent = "        ";
        let inner_w = field + 3; // label + 2 spaces + value field
        let bar = style::fg("\u{2502}", C_BOX);
        let mut f = String::new();
        // Boxed stack — a little calculator "card".
        f.push_str(&format!(
            "{}{}\n",
            indent,
            style::fg(
                &format!(
                    "\u{256d}\u{2500} Stack {}\u{256e}",
                    "\u{2500}".repeat(inner_w.saturating_sub(6))
                ),
                C_BOX
            )
        ));
        for (label, val, is_x) in [
            ("L", &d.last_x, false),
            ("T", &d.t, false),
            ("Z", &d.z, false),
            ("Y", &d.y, false),
            ("X", &d.x, true),
        ] {
            let pad = field.saturating_sub(crust::display_width(val));
            let lab = if is_x {
                style::bold(&style::fg(label, C_TITLE))
            } else {
                style::fg(label, C_LABEL)
            };
            let value = format!("{}{}", " ".repeat(pad), val);
            let value = if is_x {
                style::bold(&style::fg(&value, C_XVAL))
            } else {
                style::fg(&value, C_VAL)
            };
            f.push_str(&format!("{}{} {}  {} {}\n", indent, bar, lab, value, bar));
        }
        f.push_str(&format!(
            "{}{}\n",
            indent,
            style::fg(
                &format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(inner_w + 2)),
                C_BOX
            )
        ));
        // Alpha register, only when it holds something.
        if !d.alpha.is_empty() {
            f.push_str(&format!(
                "{}  {} {}\n",
                indent,
                style::fg("\u{03b1}", C_LABEL),
                style::fg(&d.alpha, C_MODE)
            ));
        }
        f.push('\n');
        f.push_str(&self.legend());
        self.main.set_text(&f);
        self.main.ix = 0;
        self.main.full_refresh();
    }

    /// The function legend: trigger key in orange, description dim, grouped by
    /// category and laid on a fixed column grid so cells line up vertically.
    /// Everything not on a direct key is reachable via the `:` command palette
    /// (listed at the bottom), so all XRPN functions are shown here.
    fn legend(&self) -> String {
        const COL: usize = 11; // grid column width (display cols)
        let dm = |t: &str| style::fg(t, C_DESC);
        // A grid cell (key in colour `kc`, description dim) padded to COL by its
        // *plain* width (ANSI not counted), so cells line up vertically.
        let cellc = |kc: u8, k: &str, t: &str| -> String {
            let plain = crust::display_width(k)
                + if t.is_empty() { 0 } else { 1 + crust::display_width(t) };
            let styled = if t.is_empty() {
                style::fg(k, kc)
            } else {
                format!("{} {}", style::fg(k, kc), dm(t))
            };
            format!("{}{}", styled, " ".repeat(COL.saturating_sub(plain)))
        };
        let grp = |label: &str, cells: &[(&str, &str)]| -> String {
            let row: String = cells.iter().map(|(k, t)| cellc(C_KEY, k, t)).collect();
            format!("  {}   {}\n", style::fg(&format!("{:>5}", label), C_GRP), row)
        };
        let mut out = String::new();
        // Active shift page (coloured) shown on top, so its extra functions are
        // obvious; the base legend below still lists everything as usual.
        if self.page != 0 {
            let p = &SHIFT_PAGES[self.page - 1];
            let row: String = p.keys.iter().map(|(k, l, _)| cellc(p.color, k, l)).collect();
            out.push_str(&format!(
                "  {}   {}\n\n",
                style::bold(&style::fg(&format!("{:>5}", format!("SH {}", p.tag)), p.color)),
                row
            ));
        }
        out.push_str(&grp(
            "entry",
            &[
                ("0-9", ""),
                (".", ""),
                ("e", "eex"),
                ("h", "\u{00b1}"),
                ("\u{21b5}", "push"),
                ("\u{232b}", "back"),
            ],
        ));
        out.push_str(&grp(
            "stack",
            &[
                ("\u{2190}", "x\u{21c4}y"),
                ("\u{2191}\u{2193}", "roll"),
                ("l", "LASTx"),
                ("c", "CLx"),
                ("C", "CLstk"),
            ],
        ));
        out.push_str(&grp(
            "arith",
            &[("+ \u{2212} \u{00d7} \u{00f7}", ""), ("\\", "mod"), ("%", "pct")],
        ));
        out.push_str(&grp(
            "power",
            &[
                ("q", "\u{221a}"),
                ("x", "1/x"),
                ("^", "y\u{02e3}"),
                ("p", "\u{03c0}"),
                ("|", "|x|"),
                ("!", "fact"),
            ],
        ));
        out.push_str(&grp(
            "trig",
            &[
                ("i", "sin"),
                ("o", "cos"),
                ("a", "tan"),
                ("IOA", "arc"),
                ("r", "rad"),
                ("d", "deg"),
            ],
        ));
        out.push_str(&grp("log", &[("n", "ln"), ("g", "log")]));
        out.push_str(&grp(
            "mode",
            &[("f", "fix"), ("s", "sci"), ("'", "fmt"), ("u", "undo")],
        ));
        out.push_str(&grp("reg", &[("S", "sto"), ("R", "rcl")]));
        out.push('\n');
        // The rest of the phone's shift-page functions, reached by typing the
        // command name after `:`.
        out.push_str(&format!(
            "  {}   {}\n",
            style::fg(&format!("{:>5}", ":"), C_KEY),
            dm("type any command \u{2014} sqr cube e\u{02e3} 10\u{02e3} \u{02e3}\u{221a}y  asin acos atan  \u{03a3}+ \u{03a3}\u{2212} mean sdev CL\u{03a3}")
        ));
        out.push_str(&format!(
            "  {}   {}\n",
            " ".repeat(5),
            dm("abs int frc rnd drop \u{0394}%   eng grad   hms hr \u{2192}P \u{2192}R")
        ));
        out
    }

    fn render_foot(&mut self) {
        if self.msg.is_empty() {
            self.foot.say(&style::fg(
                " number then ENTER to push \u{00b7} ops act on Y and X \u{00b7} ':' any command \u{00b7} 'u' undo \u{00b7} 'Q' quit",
                C_DESC,
            ));
        } else {
            let m = self.msg.clone();
            self.foot.say(&format!(" {}", style::fg(&m, C_MSG)));
        }
    }

    fn render_all(&mut self) {
        self.render_top();
        self.render_main();
        self.render_foot();
    }

    // ---- main loop -------------------------------------------------------

    /// Returns the X-register display string to emit on a normal quit, or
    /// None on a cancel-quit (Ctrl+C).
    fn run(&mut self) -> Option<String> {
        self.render_all();
        loop {
            let Some(key) = Input::getchr(None) else { continue };
            self.msg.clear();
            // Active shift page overrides its keys (and stays active); every
            // other key falls through to the base handling below.
            if let Some(cmd) = self.page_cmd(&key) {
                self.run_cmd(cmd);
                self.render_all();
                continue;
            }
            match key.as_str() {
                // ----- shift / quit
                "TAB" => self.page = (self.page + 1) % (SHIFT_PAGES.len() + 1),
                "Q" => return Some(display(self.state.clone()).x),
                "ESC" => {
                    if self.page != 0 {
                        self.page = 0; // un-shift instead of quitting
                    } else {
                        return Some(display(self.state.clone()).x);
                    }
                }
                "C-C" => return None, // cancel: emit nothing
                // ----- number entry
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                    self.push_undo();
                    self.state = key_digit(self.state.clone(), key.clone());
                }
                "." | "," => {
                    self.push_undo();
                    self.state = key_dot(self.state.clone());
                }
                "e" => {
                    self.push_undo();
                    self.state = key_eex(self.state.clone());
                }
                "BACK" | "C-H" => {
                    self.push_undo();
                    self.state = key_backspace(self.state.clone());
                }
                "h" => {
                    self.push_undo();
                    self.state = key_chs(self.state.clone());
                }
                "ENTER" => {
                    self.push_undo();
                    self.state = key_enter(self.state.clone());
                }
                // ----- arithmetic
                "+" => self.run_cmd("add"),
                "-" => self.run_cmd("subtract"),
                "*" => self.run_cmd("multiply"),
                "/" => self.run_cmd("divide"),
                "\\" => self.run_cmd("mod"),
                "%" => self.run_cmd("percent"),
                // ----- stack
                "<" | "LEFT" | "RIGHT" => self.run_cmd("swap"),
                "UP" => self.run_cmd("rup"),
                "DOWN" => self.run_cmd("rdn"),
                "l" => self.run_cmd("lastx"),
                "c" => self.run_cmd("clx"),
                // ----- math / trig / log
                "p" => self.run_cmd("pi"),
                "^" => self.run_cmd("pow"),
                "x" => self.run_cmd("recip"),
                "q" => self.run_cmd("sqrt"),
                "n" => self.run_cmd("ln"),
                "g" => self.run_cmd("log"),
                "i" => self.run_cmd("sin"),
                "o" => self.run_cmd("cos"),
                "a" => self.run_cmd("tan"),
                // Arc functions on Shift (Ctrl+I/M/H are eaten by the terminal
                // as TAB/ENTER/BACK, so Ctrl+trig can't work).
                "I" => self.run_cmd("asin"),
                "O" => self.run_cmd("acos"),
                "A" => self.run_cmd("atan"),
                "r" => self.run_cmd("rad"),
                "d" => self.run_cmd("deg"),
                "!" | "C-F" => self.run_cmd("fact"),
                "|" => self.run_cmd("abs"),
                // ----- command palette: any XRPN function by name (sqr, cube,
                // exp, tenx, root, asin.., splus, mean, sdev, hms, r_p, eng, ...)
                ":" => {
                    let cmd = self.ask("cmd: ");
                    if !cmd.is_empty() {
                        self.run_cmd(&cmd);
                    }
                }
                // ----- clear stack
                // (Ctrl+C is quit-cancel above; use C-x style clear via 'C')
                "C" => self.run_cmd("clst"),
                // ----- registers + modes (prompted)
                "S" => {
                    let r = self.ask("STO register: ");
                    if !r.is_empty() {
                        self.run_cmd(&format!("sto {}", r));
                    }
                }
                "R" => {
                    let r = self.ask("RCL register: ");
                    if !r.is_empty() {
                        self.run_cmd(&format!("rcl {}", r));
                    }
                }
                "f" => {
                    let n = self.ask("FIX decimals: ");
                    if !n.is_empty() {
                        self.run_cmd(&format!("fix {}", n));
                    }
                }
                "s" => {
                    let n = self.ask("SCI decimals: ");
                    if !n.is_empty() {
                        self.run_cmd(&format!("sci {}", n));
                    }
                }
                // ----- number format toggle (flag 28: comma vs dot decimal)
                "'" => {
                    self.push_undo();
                    let cur = *self.state.flags.get("28").unwrap_or(&false);
                    self.state.flags.insert("28".to_string(), !cur);
                }
                // ----- undo
                "u" => {
                    if let Some(prev) = self.undo.pop() {
                        self.state = prev;
                    } else {
                        self.msg = "Nothing to undo".to_string();
                    }
                }
                "H" => {
                    self.msg =
                        "Keys shown below the stack. STO/RCL/FIX/SCI prompt for a number.".to_string();
                }
                _ => {}
            }
            self.render_all();
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("RPNx \u{2014} terminal RPN / XRPN scientific calculator");
        println!("  Digits, '.', 'e' (eex), 'h' (\u{00b1}), ENTER to push.");
        println!("  + - * /  \\ (mod)  % (pct)   <  x\u{21c4}y   \u{2191}\u{2193} roll   l LASTx");
        println!("  q \u{221a}  x 1/x  ^ y\u{02e3}   n ln  g log   p \u{03c0}   i/o/a sin/cos/tan (Ctrl = arc)");
        println!("  r rad  d deg   f fix  s sci   S sto  R rcl   ' number-format   ! fact   | abs");
        println!("  : type any XRPN command (sqr cube exp tenx root  \u{03a3}+ mean sdev  hms \u{2192}P eng grad \u{2026})");
        println!("  TAB cycle shift pages f/g/h (coloured): powers / stats / modes+convert; ESC = base");
        println!("  u undo   c CLx   C CLstk   H help   Q quit");
        println!();
        println!("  --emit-x   print the X register to stdout on quit (for scribe paste-back)");
        return;
    }
    let emit_x = args.iter().any(|a| a == "--emit-x");

    Crust::init();
    Crust::set_app_identity("rpnx");
    let mut app = App::new();
    let result = app.run();
    save_state(&app.state);
    Crust::cleanup();
    if emit_x {
        if let Some(x) = result {
            println!("{}", x.trim());
        }
    }
}
