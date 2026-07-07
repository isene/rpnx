// The bundled TVM program is a real HP-style solver: each variable key stores
// (when a number was just keyed -> flag 22 set) or solves for that variable
// (flag 22 clear) from the other four. This drives it exactly as the TUI does.

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

/// Key a value then press the variable key (flag 22 set -> store).
fn store(mut s: CalcState, p: &Program, name: &str, v: f64) -> CalcState {
    s.x = v;
    s.entering = String::new();
    s.flags.insert("22".into(), true);
    run_program(s, p.clone(), label_line(p, name), vec![], false, 0).calc
}

/// Press the variable key with nothing keyed (flag 22 clear -> solve).
fn solve_x(mut s: CalcState, p: &Program, name: &str) -> f64 {
    s.flags.insert("22".into(), false);
    run_program(s, p.clone(), label_line(p, name), vec![], false, 0).calc.x
}

#[test]
fn tvm_solver_all_directions() {
    let p = parse_program("tvm".into(), include_str!("../examples/tvm.xrpn").into());
    let (n, i, pv, pmt) = (10.0, 5.0, -1000.0, -100.0);

    // Store the four knowns, solve FV.
    let mut s = new_state();
    s = store(s, &p, "N", n);
    s = store(s, &p, "I", i);
    s = store(s, &p, "PV", pv);
    s = store(s, &p, "PMT", pmt);
    let fv = solve_x(s.clone(), &p, "FV");
    assert!((fv - 2886.683881).abs() < 1e-3, "FV = {fv}");

    // With N/I/PV/PMT/FV all set, each other variable must solve back.
    let base = store(s, &p, "FV", fv);
    assert!((solve_x(base.clone(), &p, "PV") - pv).abs() < 1e-3);
    assert!((solve_x(base.clone(), &p, "PMT") - pmt).abs() < 1e-3);
    assert!((solve_x(base.clone(), &p, "N") - n).abs() < 1e-3);
    let solved_i = solve_x(base.clone(), &p, "I"); // secant iteration
    assert!((solved_i - i).abs() < 1e-3, "I = {solved_i}");
}
