// Verifies the bundled TVM example program computes a correct future value,
// driven exactly the way the TUI drives it: set X, run a global label.

use rpnx_core::{new_state, parse_program, run_program, CalcState, Program};

fn label_line(p: &Program, name: &str) -> u32 {
    for (i, l) in p.lines.iter().enumerate() {
        let t = l.trim();
        let mut it = t.splitn(2, char::is_whitespace);
        if it.next().unwrap_or("").eq_ignore_ascii_case("lbl") {
            let arg = it.next().unwrap_or("").trim().trim_matches('"');
            if arg.eq_ignore_ascii_case(name) {
                return i as u32;
            }
        }
    }
    panic!("label {name} not found");
}

fn run_label(mut s: CalcState, p: &Program, name: &str, x: f64) -> CalcState {
    s.x = x;
    s.entering = String::new();
    run_program(s, p.clone(), label_line(p, name), vec![], false, 0).calc
}

#[test]
fn tvm_future_value() {
    let text = include_str!("../examples/tvm.xrpn");
    let p = parse_program("tvm".into(), text.into());

    let mut s = new_state();
    s = run_label(s, &p, "N", 10.0);
    s = run_label(s, &p, "I", 5.0);
    s = run_label(s, &p, "PV", -1000.0);
    s = run_label(s, &p, "PMT", 0.0);
    // FV takes no fresh input; keep X as-is.
    let pc = label_line(&p, "FV");
    let fv = run_program(s, p.clone(), pc, vec![], false, 0).calc.x;

    // -PV·1.05^10 = 1000 · 1.6288946267… = 1628.8946…
    assert!((fv - 1628.8946267).abs() < 1e-4, "FV was {fv}");
}
