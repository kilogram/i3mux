//! Tabbed and stacked containers of i3mux terminals, one screenshot per
//! tab. The first terminal is the shell prompt that `activate` opens.

use rstest::rstest;

use super::common::{ComparisonSpec, Result, Session, TestEnvironment};

const WS: u32 = 20;

/// Activate, switch to `layout`, add `colors`, then screenshot each child
/// from the first, moving with `focus {direction}`.
fn containers(
    session: Session,
    layout: &str,
    colors: &[&str],
    direction: &str,
    golden: &str,
) -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(session, WS)?;
    env.sway(&format!("layout {layout}"))?;
    for color in colors {
        env.launch_color(color)?;
    }
    assert_eq!(env.marks(WS)?.len(), colors.len() + 1);
    let back = if direction == "right" { "left" } else { "up" };
    for _ in colors {
        env.sway(&format!("focus {back}"))?;
    }
    let shots = env.capture_each(colors.len() + 1, direction)?;
    env.compare_each_with_golden(golden, &shots, &ComparisonSpec::load(golden)?)
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn tabbed_2_terminals(#[case] session: Session) -> Result<()> {
    containers(session, "tabbed", &["green"], "right", "tabbed-2-terminals")
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn tabbed_3_terminals(#[case] session: Session) -> Result<()> {
    containers(
        session,
        "tabbed",
        &["green", "blue"],
        "right",
        "tabbed-3-terminals",
    )
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn stacked_2_terminals(#[case] session: Session) -> Result<()> {
    containers(
        session,
        "stacking",
        &["yellow"],
        "down",
        "stacked-2-terminals",
    )
}
