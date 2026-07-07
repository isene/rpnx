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
        }
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
        let title = format!(" RPNx    {}", d.mode);
        let hint = "H help \u{00b7} Q quit ";
        let pad = (self.cols as usize)
            .saturating_sub(crust::display_width(&title) + crust::display_width(hint));
        self.top.say(&format!(
            "{}{}{}",
            style::bold(&style::fg(&title, C_TITLE)),
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
            .max(10);
        let indent = "     ";
        let mut f = String::new();
        for (label, val, is_x) in [
            ("L", &d.last_x, false),
            ("T", &d.t, false),
            ("Z", &d.z, false),
            ("Y", &d.y, false),
            ("X", &d.x, true),
        ] {
            let pad = field.saturating_sub(crust::display_width(val));
            let val_col = if is_x { C_XVAL } else { C_VAL };
            let lab = if is_x {
                style::bold(&style::fg(label, C_TITLE))
            } else {
                style::fg(label, C_LABEL)
            };
            let value = format!("{}{}", " ".repeat(pad), val);
            let value = if is_x {
                style::bold(&style::fg(&value, val_col))
            } else {
                style::fg(&value, val_col)
            };
            f.push_str(&format!("{}{}  {}\n", indent, lab, value));
        }
        // Alpha register, only when it holds something.
        if !d.alpha.is_empty() {
            f.push_str(&format!(
                "{}{}  {}\n",
                indent,
                style::fg("A", C_LABEL),
                style::fg(&d.alpha, C_MODE)
            ));
        }
        f.push('\n');
        f.push_str(&self.legend());
        self.main.set_text(&f);
        self.main.ix = 0;
        self.main.full_refresh();
    }

    /// The function-key legend: trigger key in orange, description dim.
    fn legend(&self) -> String {
        let o = |k: &str| style::fg(k, C_KEY);
        let d = |t: &str| style::fg(t, C_DESC);
        let cell = |k: &str, t: &str| format!("{} {}", o(k), d(t));
        let rows = [
            vec![
                cell("\u{2191}\u{2193}", "roll"),
                cell("\u{2190}", "x\u{21c4}y"),
                cell("+ \u{2212} \u{00d7} \u{00f7}", ""),
                cell("\\", "mod"),
                cell("%", "pct"),
                cell("h", "\u{00b1}"),
                cell("p", "\u{03c0}"),
                cell("e", "eex"),
            ],
            vec![
                cell("^", "y\u{02e3}"),
                cell("x", "1/x"),
                cell("q", "\u{221a}"),
                cell("n", "ln"),
                cell("g", "log"),
                cell("l", "LASTx"),
            ],
            vec![
                cell("i", "sin"),
                cell("o", "cos"),
                cell("a", "tan"),
                cell("r", "rad"),
                cell("d", "deg"),
                cell("c", "CLx"),
                cell("C", "CLstk"),
            ],
            vec![
                cell("S", "sto"),
                cell("R", "rcl"),
                cell("f", "fix"),
                cell("s", "sci"),
                cell("u", "undo"),
                cell("'", "fmt"),
                cell("!", "fact"),
            ],
        ];
        let mut out = String::new();
        for r in rows {
            out.push_str("   ");
            out.push_str(&r.join("   "));
            out.push('\n');
        }
        out
    }

    fn render_foot(&mut self) {
        if self.msg.is_empty() {
            self.foot.say(&style::fg(
                " Enter a number, ENTER to push. Ops act on Y and X. \u{00b7} 'u' undo \u{00b7} 'Q' quit",
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
            match key.as_str() {
                // ----- quit
                "Q" | "ESC" => return Some(display(self.state.clone()).x),
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
                "C-I" => self.run_cmd("asin"),
                "C-O" => self.run_cmd("acos"),
                "C-A" => self.run_cmd("atan"),
                "r" => self.run_cmd("rad"),
                "d" => self.run_cmd("deg"),
                "!" | "C-F" => self.run_cmd("fact"),
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
        println!("  r rad  d deg   f fix  s sci   S sto  R rcl   ' number-format   ! fact");
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
