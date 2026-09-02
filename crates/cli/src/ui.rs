//! The terminal conversation: asking, offering a numbered choice, and the
//! heading-plus-bullets shape every wizard step announces itself with.
//!
//! Hand-rolled on purpose. A prompt crate (dialoguer, inquire) would take
//! over the alternate screen, raw mode and the arrow keys, which is a lot of
//! dependency and a lot of terminal for a tool whose whole promise is "I
//! print what I am about to do and you type y". Line-reading also keeps the
//! wizard scriptable and its transcript readable in scrollback afterwards.

use std::io::Write as _;

use owo_colors::OwoColorize as _;

/// Asks a y/N question on the terminal, defaulting to no.
///
/// Callers must first check that stdin is a TTY; piped invocations should
/// require an explicit flag instead.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if the terminal cannot be read.
pub fn confirm(question: &str) -> anyhow::Result<bool> {
    ask(question, "[y/N]", false)
}

/// Asks a Y/n question on the terminal, defaulting to yes.
///
/// The wizard's shape: every step has a recommended answer, and pressing
/// enter takes it.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if the terminal cannot be read.
pub fn confirm_default_yes(question: &str) -> anyhow::Result<bool> {
    ask(question, "[Y/n]", true)
}

fn ask(question: &str, hint: &str, default: bool) -> anyhow::Result<bool> {
    print!("{question} {hint} ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(parse_yes_no(&line).unwrap_or(default))
}

/// `None` for anything that is not recognisably a yes or a no — an empty
/// line, but also a typo, which takes the default rather than the risk.
fn parse_yes_no(line: &str) -> Option<bool> {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// Offers `options` as a numbered list and returns the index picked. An
/// empty line takes `default`; anything unparseable is asked again.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if the terminal cannot be read.
pub fn choose(prompt: &str, options: &[String], default: usize) -> anyhow::Result<usize> {
    for (i, option) in options.iter().enumerate() {
        println!("  {}) {option}", i + 1);
    }
    loop {
        print!("{prompt} [{}] ", default + 1);
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            // EOF: no more answers are coming, so re-asking would spin.
            return Ok(default);
        }
        if let Some(picked) = parse_choice(&line, options.len(), default) {
            return Ok(picked);
        }
        println!("  pick a number between 1 and {}", options.len());
    }
}

fn parse_choice(line: &str, len: usize, default: usize) -> Option<usize> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Some(default);
    }
    trimmed
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=len).contains(n))
        .map(|n| n - 1)
}

/// Prints a step's announcement: a bold heading, then one indented bullet
/// per thing the step is about to do or has just found.
pub fn step(heading: &str, bullets: &[String], color: bool) {
    if color {
        println!("{}", heading.bold());
    } else {
        println!("{heading}");
    }
    for bullet in bullets {
        println!("  {bullet}");
    }
}

/// Prints a "there was nothing to do here" line, dimmed the way `list`
/// dims a disabled server: present in the transcript, not competing for
/// attention with the steps that are actually asking something.
pub fn already_done(text: &str, color: bool) {
    if color {
        println!("{}", text.dimmed());
    } else {
        println!("{text}");
    }
}

/// Dims `text` for inline use inside a line that is not itself dimmed.
#[must_use]
pub fn dim(text: &str, color: bool) -> String {
    if color {
        text.dimmed().to_string()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_real_yes_or_no_overrides_the_default() {
        assert_eq!(parse_yes_no("y\n"), Some(true));
        assert_eq!(parse_yes_no(" YES \n"), Some(true));
        assert_eq!(parse_yes_no("N"), Some(false));
        assert_eq!(parse_yes_no("no"), Some(false));
        assert_eq!(parse_yes_no("\n"), None);
        assert_eq!(parse_yes_no("maybe"), None);
    }

    #[test]
    fn a_choice_is_one_based_and_bounded() {
        assert_eq!(parse_choice("1\n", 3, 2), Some(0));
        assert_eq!(parse_choice(" 3 ", 3, 2), Some(2));
        assert_eq!(parse_choice("\n", 3, 1), Some(1));
        assert_eq!(parse_choice("0", 3, 1), None);
        assert_eq!(parse_choice("4", 3, 1), None);
        assert_eq!(parse_choice("both", 3, 1), None);
    }

    #[test]
    fn dimming_is_opt_in() {
        assert!(dim("x", true).contains('\u{1b}'));
        assert_eq!(dim("x", false), "x");
    }
}
