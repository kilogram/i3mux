//! Nested containers of i3mux terminals: tabs or stacks inside splits,
//! splits inside tabs, and one deeper mix. The first terminal (top left) is
//! the shell prompt that `activate` opens.

use rstest::rstest;

use super::common::{ComparisonSpec, Result, Session, TestEnvironment};

const WS: u32 = 23;

/// Run `steps` (Sway commands, or `+color` to add a terminal), check the
/// terminal count, then run `shots`: `=` captures, anything else is a Sway
/// command. Compares the captures with `{golden}-N.png`.
fn build_and_capture(
    session: Session,
    steps: &[&str],
    terminals: usize,
    shots: &[&str],
    golden: &str,
) -> Result<()> {
    let env = TestEnvironment::new()?;
    env.activate(session, WS)?;
    for step in steps {
        match step.strip_prefix('+') {
            Some(color) => env.launch_color(color)?,
            None => env.sway(step)?,
        }
    }
    assert_eq!(env.marks(WS)?.len(), terminals);
    let mut captured = Vec::new();
    for step in shots {
        if *step == "=" {
            captured.push(env.capture_screenshot()?);
        } else {
            env.sway(step)?;
        }
    }
    env.compare_each_with_golden(golden, &captured, &ComparisonSpec::load(golden)?)
}

/// `[tabs(prompt, green)] | [blue]`
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn tabs_in_hsplit(#[case] session: Session) -> Result<()> {
    build_and_capture(
        session,
        &[
            "layout tabbed",
            "+green",
            "focus parent",
            "split h",
            "+blue",
        ],
        3,
        &["focus left", "focus left", "=", "focus right", "="],
        "tabs-in-hsplit",
    )
}

/// `[tabs(prompt, green)] / [blue]`
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn tabs_in_vsplit(#[case] session: Session) -> Result<()> {
    build_and_capture(
        session,
        &[
            "layout tabbed",
            "+green",
            "focus parent",
            "split v",
            "+blue",
        ],
        3,
        &["focus up", "focus left", "=", "focus right", "="],
        "tabs-in-vsplit",
    )
}

/// `tabs(prompt, green, [blue | yellow])`: the first `split h` splits the
/// workspace itself, so `layout tabbed` turns prompt and green into tabs.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn hsplit_in_tabs(#[case] session: Session) -> Result<()> {
    build_and_capture(
        session,
        &[
            "split h",
            "+green",
            "focus parent",
            "layout tabbed",
            "+blue",
            "split h",
            "+yellow",
        ],
        4,
        // yellow -> blue -> green tab -> prompt tab; then right to the split.
        &[
            "focus left",
            "focus left",
            "focus left",
            "=",
            "focus right",
            "focus right",
            "=",
        ],
        "hsplit-in-tabs",
    )
}

/// `tabs(prompt, green, [blue / yellow])`, built as in `hsplit_in_tabs`.
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn vsplit_in_tabs(#[case] session: Session) -> Result<()> {
    build_and_capture(
        session,
        &[
            "split v",
            "+green",
            "focus parent",
            "layout tabbed",
            "+blue",
            "split v",
            "+yellow",
        ],
        4,
        &[
            "focus parent",
            "focus up",
            "focus left",
            "=",
            "focus right",
            "=",
        ],
        "vsplit-in-tabs",
    )
}

/// `[stack(prompt, green)] | [blue]`
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn stacked_in_hsplit(#[case] session: Session) -> Result<()> {
    build_and_capture(
        session,
        &[
            "layout stacking",
            "+green",
            "focus parent",
            "split h",
            "+blue",
        ],
        3,
        &["focus left", "focus up", "=", "focus down", "="],
        "stacked-in-hsplit",
    )
}

/// `vsplit(hsplit(tabs(prompt, green), blue), hsplit(yellow, magenta))`
#[rstest]
#[case::local(Session::Local)]
#[case::remote(Session::Remote)]
fn complex_nested_layout(#[case] session: Session) -> Result<()> {
    build_and_capture(
        session,
        &[
            "layout tabbed",
            "+green",
            "focus parent",
            "split h",
            "+blue",
            "focus parent",
            "split v",
            "+yellow",
            "split h",
            "+magenta",
        ],
        5,
        &[
            "focus up",
            "focus left",
            "focus left",
            "=",
            "focus right",
            "=",
        ],
        "complex-nested",
    )
}
