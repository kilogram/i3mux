//! Two i3mux terminals side by side and stacked, local and remote.
//! Local and remote share one golden: the shells look the same.

use rstest::rstest;

use super::common::{ComparisonSpec, Result, Session, TestEnvironment};

const WS: u32 = 1;

fn two_terminals(session: Session, split: &str, color: &str, golden: &str) -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(session, WS)?;
    env.sway(split)?;
    env.launch_color(color)?;
    assert_eq!(
        env.marks(WS)?,
        [session.mark("ws1", 1), session.mark("ws1", 2)]
    );
    let shot = env.capture_screenshot()?;
    env.compare_with_golden(
        &format!("{golden}.png"),
        &shot,
        &ComparisonSpec::load(golden)?,
    )
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn hsplit_2_terminals(#[case] session: Session) -> Result<()> {
    two_terminals(session, "split h", "green", "hsplit-2-terminals")
}

#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn vsplit_2_terminals(#[case] session: Session) -> Result<()> {
    two_terminals(session, "split v", "yellow", "vsplit-2-terminals")
}
