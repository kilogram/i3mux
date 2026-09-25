//! Layout restore, driven by the specs in `golden/specs/restore-*.toml`.
//!
//! Each spec builds a layout of i3mux terminals. The screenshot before
//! `detach` must match the golden, and the screenshot after `attach` must
//! match the one before `detach` (in every mode, including
//! `UPDATE_GOLDENS`), for a local and a remote session.

use rstest::rstest;

use super::common::{assert_same_screen, ComparisonSpec, Result, Session, TestEnvironment};

const WS: u32 = 4;

#[rstest]
#[case("restore-hsplit-2")]
#[case("restore-vsplit-2")]
#[case("restore-tabbed-2")]
#[case("restore-tabbed-3")]
#[case("restore-stacked-2")]
#[case("restore-3way-hsplit")]
#[case("restore-3way-vsplit")]
#[case("restore-tabs-in-hsplit")]
#[case("restore-hsplit-in-tabs")]
#[case("restore-vsplit-in-tabs")]
fn layout_survives_detach_and_attach(
    #[case] spec_name: &str,
    #[values(Session::Local, Session::Remote)] session: Session,
) -> Result<()> {
    let spec = ComparisonSpec::load(spec_name)?;
    let golden = format!("{spec_name}.png");
    let env = TestEnvironment::new()?;

    env.activate(session, WS)?;
    env.exec_actions(&spec.actions)?;
    let marks = env.marks(WS)?;
    assert_eq!(marks.len(), spec.terminal_count, "terminals before detach");
    env.exec_actions(&spec.pre_screenshot)?;
    let before = env.capture_screenshot()?;
    env.compare_with_golden(&golden, &before, &spec)?;

    env.i3mux().detach()?;
    env.wait_for_window_count(WS, 0)?;

    env.i3mux().attach(session, "")?.ok()?;
    assert_eq!(env.marks(WS)?, marks, "attach restores the same terminals");

    env.exec_actions(&spec.pre_screenshot)?;
    assert_same_screen(&before, &env.capture_screenshot()?, &spec)
}
